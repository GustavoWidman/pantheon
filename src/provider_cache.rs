//! Shared request coordination, following uniichat 0500bf6 AskAI.wait/lead.
//! Cache residency is only a hint, never evidence of a provider cache hit.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;

type Key = [u8; 32];
#[derive(Default)]
pub(super) struct Coordinator {
    entries: Mutex<VecDeque<(Key, Entry)>>,
}
struct Entry {
    token: Arc<()>,
    until: Instant,
    writing: Option<watch::Sender<bool>>,
}
/// Drop releases a failed/cancelled leader; cancellation of a waiter owns no slot.
pub(super) struct Flight {
    coordinator: Arc<Coordinator>,
    key: Key,
    token: Arc<()>,
    ttl: Duration,
}
impl Coordinator {
    pub(super) async fn wait(self: &Arc<Self>, keys: &[Key], ttl: Duration) -> Option<Flight> {
        self.wait_at(keys, ttl, Instant::now).await
    }
    async fn wait_at(
        self: &Arc<Self>,
        keys: &[Key],
        ttl: Duration,
        now: impl Fn() -> Instant,
    ) -> Option<Flight> {
        let last = *keys.last()?;
        loop {
            let mut waiting = {
                let now = now();
                let mut entries = self.entries.lock().unwrap();
                let got = keys.iter().enumerate().rev().find_map(|(k, key)| {
                    entries
                        .iter()
                        .position(|(id, e)| id == key && (e.writing.is_some() || e.until > now))
                        .map(|i| (k, i))
                });
                if let Some((_, i)) = got.filter(|(_, i)| entries[*i].1.writing.is_some()) {
                    Some(entries[i].1.writing.as_ref().unwrap().subscribe())
                } else {
                    if let Some((k, i)) = got {
                        let (key, mut entry) = entries.remove(i).unwrap();
                        entry.until = now + ttl;
                        entries.push_back((key, entry));
                        if k == keys.len() - 1 {
                            return None;
                        }
                    }
                    // Reference eviction: remove the expired LRU prefix only.
                    while entries
                        .front()
                        .is_some_and(|(_, e)| e.writing.is_none() && e.until < now)
                    {
                        entries.pop_front();
                    }
                    let token = Arc::new(());
                    let (ready, _) = watch::channel(false);
                    let entry = Entry {
                        token: token.clone(),
                        until: now,
                        writing: Some(ready),
                    };
                    // Map.set replaces an expired key without changing its
                    // insertion position when a live LRU head prevented eviction.
                    if let Some((_, old)) = entries.iter_mut().find(|(key, _)| *key == last) {
                        *old = entry;
                    } else {
                        entries.push_back((last, entry));
                    }
                    return Some(Flight {
                        coordinator: self.clone(),
                        key: last,
                        token,
                        ttl,
                    });
                }
            };
            if let Some(waiting) = waiting.as_mut() {
                // Subscribe under the state lock, so a fast leader cannot lose a wakeup.
                let _ = waiting.changed().await;
            }
        }
    }
}
impl Flight {
    pub(super) fn begun(&self) {
        self.begun_at(Instant::now());
    }
    fn begun_at(&self, now: Instant) {
        let mut entries = self.coordinator.entries.lock().unwrap();
        if let Some((_, entry)) = entries
            .iter_mut()
            .find(|(key, e)| *key == self.key && Arc::ptr_eq(&e.token, &self.token))
            && let Some(ready) = entry.writing.take()
        {
            entry.until = now + self.ttl;
            let _ = ready.send(true);
        }
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        let mut entries = self.coordinator.entries.lock().unwrap();
        if let Some(i) = entries.iter().position(|(key, e)| {
            *key == self.key && Arc::ptr_eq(&e.token, &self.token) && e.writing.is_some()
        }) {
            let (_, entry) = entries.remove(i).unwrap();
            if let Some(ready) = entry.writing {
                let _ = ready.send(true);
            }
        }
    }
}

/// Prefix keys use only supported *actual wire* marks, never hypothetical Codex
/// or Z.ai cache entries. SHA-256 replaces Bun.hash; text/control bytes matter,
/// not hash spelling. The routing identity scopes private durable cache domains.
pub(super) fn keys(
    model: &str,
    body: &Value,
    identity: uuid::Uuid,
) -> Option<(Vec<Key>, Duration)> {
    let (field, ttl, mark) = if model.starts_with("anthropic/") {
        ("messages", Duration::from_secs(300), "cache_control")
    } else if model.starts_with("openai/") {
        (
            "input",
            Duration::from_secs(1800),
            "prompt_cache_breakpoint",
        )
    } else {
        return None;
    };
    let messages = body[field].as_array()?;
    // Native view is the first user message. Subsequent turns/tools cannot
    // change a cached prefix, while all preceding system items are key material.
    let at = messages.iter().position(|m| m["role"] == "user")?;
    let blocks = messages[at]["content"].as_array()?;
    let last = blocks
        .iter()
        .take(blocks.len().saturating_sub(1))
        .rposition(|b| b.get(mark).is_some())?;
    let mut head = body.clone();
    head.as_object_mut()?.remove(field);
    for field in ["max_tokens", "max_output_tokens", "stream"] {
        head.as_object_mut()?.remove(field);
    }
    let mut hash = Sha256::new();
    hash.update(b"pantheon-cache-coordination-v1\0");
    hash.update(identity.as_bytes());
    hash.update(model.as_bytes());
    hash.update(serde_json::to_vec(&(head, &messages[..at], &messages[at]["role"])).ok()?);
    let mut out = Vec::new();
    for block in &blocks[..=last] {
        let mut block = block.clone();
        if let Some(map) = block.as_object_mut() {
            map.remove("cache_control");
            map.remove("prompt_cache_breakpoint");
        }
        let bytes = serde_json::to_vec(&block).ok()?;
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        let key: Key = hash.clone().finalize().into();
        out.push(key);
    }
    Some((out, ttl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    async fn pending<F: Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        poll_fn(|cx| {
            assert!(
                future.as_mut().poll(cx).is_pending(),
                "waiter unexpectedly sent a request"
            );
            Poll::Ready(())
        })
        .await;
    }
    fn key(n: u8) -> Key {
        [n; 32]
    }
    const TTL: Duration = Duration::from_secs(300);
    #[tokio::test]
    async fn same_prefix_waits_until_begun_not_until_leader_end() {
        let c = Arc::new(Coordinator::default());
        let leader = c.wait(&[key(1)], TTL).await.unwrap();
        let keys = [key(1)];
        let waiter = c.wait(&keys, TTL);
        tokio::pin!(waiter);
        pending(waiter.as_mut()).await;
        leader.begun();
        assert!(waiter.await.is_none());
        assert!(c.wait(&[key(1)], TTL).await.is_none());
    }
    #[tokio::test]
    async fn longest_cached_prefix_and_extensions_share_one_new_writer() {
        let c = Arc::new(Coordinator::default());
        let leader = c.wait(&[key(1)], TTL).await.unwrap();
        let keys = [key(1), key(2)];
        let waiter = c.wait(&keys, TTL);
        tokio::pin!(waiter);
        pending(waiter.as_mut()).await;
        leader.begun();
        let extension = waiter.await.unwrap();
        let keys = [key(1), key(2), key(3)];
        let sibling = c.wait(&keys, TTL);
        tokio::pin!(sibling);
        pending(sibling.as_mut()).await;
        extension.begun();
        assert!(sibling.await.is_some());
    }
    #[tokio::test]
    async fn cancelled_or_failed_leader_releases_waiter_to_lead() {
        let c = Arc::new(Coordinator::default());
        let leader = c.wait(&[key(1)], TTL).await.unwrap();
        let keys = [key(1)];
        let waiter = c.wait(&keys, TTL);
        tokio::pin!(waiter);
        pending(waiter.as_mut()).await;
        drop(leader);
        let replacement = waiter.await.unwrap();
        drop(replacement);
        assert!(c.entries.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn dropped_waiter_does_not_own_or_delete_leader() {
        let c = Arc::new(Coordinator::default());
        let leader = c.wait(&[key(1)], TTL).await.unwrap();
        {
            let keys = [key(1)];
            let waiter = c.wait(&keys, TTL);
            tokio::pin!(waiter);
            pending(waiter.as_mut()).await;
        }
        assert_eq!(c.entries.lock().unwrap().len(), 1);
        leader.begun();
        assert!(c.wait(&[key(1)], TTL).await.is_none());
    }
    #[tokio::test]
    async fn ttl_refresh_expiry_and_lru_eviction_match_reference_policy() {
        let c = Arc::new(Coordinator::default());
        let now = Instant::now();
        let a = c.wait_at(&[key(1)], TTL, || now).await.unwrap();
        a.begun_at(now);
        let b = c.wait_at(&[key(2)], TTL, || now).await.unwrap();
        b.begun_at(now);
        assert!(c.wait_at(&[key(1)], TTL, || now + TTL / 2).await.is_none());
        assert_eq!(c.entries.lock().unwrap()[1].0, key(1));
        let next = now + TTL + Duration::from_secs(1);
        let d = c.wait_at(&[key(3)], TTL, || next).await.unwrap();
        assert_eq!(c.entries.lock().unwrap().len(), 2); // expired b removed, a retained
        d.begun_at(next);
        let expired = c.wait_at(&[key(1)], TTL, || now + TTL * 2).await.unwrap();
        drop(expired);
    }
    #[tokio::test]
    async fn no_mark_has_no_reservation_and_unrelated_prefixes_do_not_wait() {
        let c = Arc::new(Coordinator::default());
        assert!(c.wait(&[], TTL).await.is_none());
        let _a = c.wait(&[key(1)], TTL).await.unwrap();
        let _b = c.wait(&[key(2)], TTL).await.unwrap();
    }
    #[test]
    fn wire_keys_preserve_prefix_reuse_and_isolate_controls_and_transport() {
        let identity = uuid::Uuid::nil();
        let history = Provider::start(
            "anthropic",
            "<chat>\n0|a\n1|b\n2|c\n3|d\n</chat>",
            "task one",
        );
        let body = Provider::request_body("anthropic/claude-test", "high", "system", &history, &[])
            .unwrap();
        let (before, ttl) = keys("anthropic/claude-test", &body, identity).unwrap();
        assert_eq!(ttl, Duration::from_secs(300));
        let mut changed = body.clone();
        changed["messages"][0]["content"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["text"] = json!("task two");
        changed["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"assistant","content":"reply"}));
        assert_eq!(
            before,
            keys("anthropic/claude-test", &changed, identity).unwrap().0
        );
        changed["system"][0]["text"] = json!("different");
        assert_ne!(
            before,
            keys("anthropic/claude-test", &changed, identity).unwrap().0
        );
        assert_ne!(
            before,
            keys("anthropic/claude-test", &body, uuid::Uuid::new_v4())
                .unwrap()
                .0
        );
        changed = body.clone();
        changed["tools"] = json!([{"name":"different"}]);
        assert_ne!(
            before,
            keys("anthropic/claude-test", &changed, identity).unwrap().0
        );
        assert!(keys("codex/gpt-6-luna", &body, identity).is_none());
        assert!(keys("zai/glm-4.7", &body, identity).is_none());
        let history = Provider::start("openai", "<chat>\n0|a\n</chat>", "task");
        let modern =
            Provider::request_body("openai/gpt-6-luna", "high", "system", &history, &[]).unwrap();
        assert_eq!(
            keys("openai/gpt-6-luna", &modern, identity).unwrap().1,
            Duration::from_secs(1800)
        );
        let old =
            Provider::request_body("openai/gpt-4.1", "none", "system", &history, &[]).unwrap();
        assert!(keys("openai/gpt-4.1", &old, identity).is_none());
    }
    // Actual Provider clones, local HTTP only. A request's admission callback
    // records the moment AFTER coordination, before any socket is submitted.
    #[tokio::test]
    async fn actual_wire_waiter_released_on_output_event_before_response_end() {
        use axum::{
            Router,
            body::{Body, Bytes},
            response::Response,
            routing::post,
        };
        use futures_util::stream::unfold;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (seen, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/",
            post(move || {
                let seen = seen.clone();
                async move {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    seen.send(tx).unwrap();
                    let stream = unfold(rx, |mut rx| async move {
                        rx.recv()
                            .await
                            .map(|s| (Ok::<_, std::io::Error>(Bytes::from(s)), rx))
                    });
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = Provider::mock(format!("http://{}/", socket.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let admitted = Arc::new(AtomicUsize::new(0));
        let history = Provider::start("openai", "<chat>\n0|same\n</chat>", "task");
        let spawn = || {
            let (p, admitted, history) = (p.clone(), admitted.clone(), history.clone());
            tokio::spawn(async move {
                p.step_observed(
                    "openai/gpt-6-luna",
                    "high",
                    "system",
                    &history,
                    &[],
                    Some(&|| {
                        admitted.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }),
                )
                .await
            })
        };
        let a = spawn();
        let first = observed.recv().await.unwrap();
        let b = spawn();
        // Observe subscription, not a timing assumption about the socket.
        for _ in 0..1000 {
            if p.cache_coordination
                .entries
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| e.writing.as_ref().is_some_and(|w| w.receiver_count() == 1))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(admitted.load(Ordering::SeqCst), 1);
        assert!(
            p.cache_coordination
                .entries
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| e.writing.as_ref().is_some_and(|w| w.receiver_count() == 1))
        );
        first
            .send("data: {\"type\":\"response.created\"}\n\n".into())
            .unwrap();
        // response.created is not OpenAI's reference ready event.
        assert_eq!(admitted.load(Ordering::SeqCst), 1);
        first
            .send("data: {\"type\":\"response.output_item.added\"}\n\n".into())
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(admitted.load(Ordering::SeqCst), 2);
        assert!(
            !a.is_finished(),
            "waiter must release before leader response ends"
        );
        let complete =
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{}}}\n\n"
                .to_owned();
        first.send(complete.clone()).unwrap();
        second.send(complete).unwrap();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        server.abort();
    }
    async fn actual_wire_leader_release(cancel: bool) {
        use axum::{Json, Router, http::StatusCode, routing::post};
        let (seen, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/",
            post(move || {
                let seen = seen.clone();
                async move {
                    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                    seen.send(tx).unwrap();
                    let fail = rx.await.unwrap_or(true);
                    (
                        if fail {
                            StatusCode::INTERNAL_SERVER_ERROR
                        } else {
                            StatusCode::OK
                        },
                        Json(json!({"content":[],"usage":{}})),
                    )
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = Provider::mock(format!("http://{}/", socket.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let history = Provider::start("anthropic", "<chat>\n0|same\n</chat>", "task");
        let spawn = || {
            let (p, history) = (p.clone(), history.clone());
            tokio::spawn(async move {
                p.step("anthropic/claude-test", "high", "system", &history, &[])
                    .await
            })
        };
        let a = spawn();
        let first = observed.recv().await.unwrap();
        let b = spawn();
        for _ in 0..1000 {
            if p.cache_coordination
                .entries
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| e.writing.as_ref().is_some_and(|w| w.receiver_count() == 1))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            p.cache_coordination
                .entries
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| e.writing.as_ref().is_some_and(|w| w.receiver_count() == 1))
        );
        if cancel {
            a.abort();
            assert!(a.await.unwrap_err().is_cancelled());
        } else {
            first.send(true).unwrap();
            assert!(a.await.unwrap().is_err());
        }
        let second = tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .unwrap()
            .unwrap();
        second.send(false).unwrap();
        b.await.unwrap().unwrap();
        assert!(
            p.cache_coordination
                .entries
                .lock()
                .unwrap()
                .iter()
                .all(|(_, e)| e.writing.is_none())
        );
        server.abort();
    }
    #[tokio::test]
    async fn actual_wire_cancelled_leader_frees_clone_waiter() {
        actual_wire_leader_release(true).await;
    }
    #[tokio::test]
    async fn actual_wire_failed_leader_frees_clone_waiter() {
        actual_wire_leader_release(false).await;
    }
    #[tokio::test]
    async fn expired_key_behind_live_lru_head_is_replaced_not_duplicated() {
        let c = Arc::new(Coordinator::default());
        let now = Instant::now();
        let active = c.wait_at(&[key(1)], TTL, || now).await.unwrap();
        let old = c.wait_at(&[key(2)], TTL, || now).await.unwrap();
        old.begun_at(now);
        let replacement = c.wait_at(&[key(2)], TTL, || now + TTL).await.unwrap();
        assert_eq!(c.entries.lock().unwrap().len(), 2);
        drop(old); // a stale guard cannot remove the replacement
        assert!(c.entries.lock().unwrap()[1].1.writing.is_some());
        replacement.begun_at(now + TTL);
        drop(active);
    }
    #[tokio::test]
    async fn sixty_four_waiters_wake_without_lost_notification_or_duplicate_leaders() {
        let c = Arc::new(Coordinator::default());
        let keys = [key(1)];
        let leader = c.wait(&keys, TTL).await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..64 {
            let mut waiter = Box::pin(c.wait(&keys, TTL));
            pending(waiter.as_mut()).await;
            waiters.push(waiter);
        }
        leader.begun();
        for waiter in waiters {
            assert!(waiter.await.is_none());
        }
    }
}
