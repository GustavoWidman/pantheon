use super::*;
use crate::store::Store;
use axum::{Json, Router, http::StatusCode, routing::get};
use std::sync::Arc;
fn fixture() -> (tempfile::TempDir, Store, Arc<Attachments>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("runtime.sqlite")).unwrap();
    let files = Arc::new(
        Attachments::open(
            dir.path(),
            dir.path(),
            AttachmentConfig {
                retention_seconds: 1,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    (dir, store, files)
}
#[tokio::test]
async fn expired_cdn_large_download_is_durable_and_channel_ordered() {
    let (dir, store, files) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/fresh");
    let size = 21 * 1024 * 1024u64;
    let descriptor = json!({"id":"200","filename":"../../large.txt","size":size,"url":url,"content_type":"text/plain"});
    let app = Router::new()
        .route("/expired", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/channels/10/messages/100",
            get(move || {
                let descriptor = descriptor.clone();
                async move { Json(json!({"attachments":[descriptor]})) }
            }),
        )
        .route(
            "/fresh",
            get(|| async {
                let chunk = axum::body::Bytes::from(vec![b'x'; 65536]);
                axum::body::Body::from_stream(futures_util::stream::iter(
                    (0..336).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
                ))
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let discord = Discord::new("private".into(), 1, vec![2])
        .unwrap()
        .mock_api(format!("http://{address}"));
    let incoming = IncomingFile {
        id: "200".into(),
        filename: "../../large.txt".into(),
        content_type: Some("text/plain".into()),
        size,
        url: format!("http://{address}/expired"),
    };
    let input = Input {
        id: "100".into(),
        channel: 10,
        user: 2,
        text: "".into(),
    };
    files
        .queue(&input, std::slice::from_ref(&incoming))
        .unwrap();
    files
        .queue(
            &Input {
                id: "101".into(),
                text: "next".into(),
                ..input.clone()
            },
            &[],
        )
        .unwrap();
    files
        .queue(
            &Input {
                id: "102".into(),
                channel: 11,
                ..input.clone()
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        files.next(&HashSet::from([10])).unwrap().unwrap().input.id,
        "102"
    );
    drop(files);
    let files =
        Arc::new(Attachments::open(dir.path(), dir.path(), AttachmentConfig::default()).unwrap());
    let pending = files.next(&HashSet::new()).unwrap().unwrap();
    assert_eq!(pending.input.id, "100");
    let admitted = files
        .receive(&pending, &discord, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!admitted.text.contains("http://"));
    assert!(admitted.text.contains("200"));
    let file = files.get(10, "100-200").unwrap();
    assert_eq!(file.size, size);
    assert!(files.copy_path(&file).starts_with(dir.path()));
    assert!(!file.filename.contains('/'));
    assert_eq!(
        tokio::fs::metadata(files.blob(&file)).await.unwrap().len(),
        size
    );
    assert!(store.admit(&admitted).unwrap());
    files.finish(&admitted.id).unwrap();
    assert_eq!(
        files.next(&HashSet::new()).unwrap().unwrap().input.id,
        "101"
    );
    assert!(!store.admit(&admitted).unwrap());
    // Expired local bytes can be restored using the retained message/attachment IDs.
    tokio::fs::remove_file(files.blob(&file)).await.unwrap();
    let reopened = files
        .reopen(10, "100-200", &discord, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reopened.hash, file.hash);
    tokio::fs::write(files.blob(&file), b"damaged original")
        .await
        .unwrap();
    assert_eq!(
        files
            .reopen(10, "100-200", &discord, &CancellationToken::new())
            .await
            .unwrap()
            .hash,
        file.hash
    );
    // Workspace collisions become a file-level failure, without dropping the prompt.
    tokio::fs::remove_file(files.copy_path(&file))
        .await
        .unwrap();
    tokio::fs::create_dir(files.copy_path(&file)).await.unwrap();
    let ready = files
        .receive(&pending, &discord, &CancellationToken::new())
        .await
        .unwrap();
    assert!(ready.text.contains("Workspace copy unavailable"));
    assert!(ready.text.contains("\"path\":null"));
    server.abort();
}
#[tokio::test]
async fn native_parts_keep_prefix_stable_and_share_provider_allowances() {
    let (dir, _store, files) = fixture();
    let path = dir.path().join("report.pdf");
    tokio::fs::write(&path, b"%PDF-test").await.unwrap();
    let mut budget = remaining_budget("openai/gpt-6-luna", &[]);
    let part = native_part(
        &path,
        "report.pdf",
        "application/pdf",
        "openai/gpt-6-luna",
        false,
        &mut budget,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(part["type"], "input_file");
    let prefix = crate::provider::Provider::start("openai", "stable memory", "first prompt");
    let mut history = prefix.clone();
    let mut user = crate::provider::Provider::user("openai", "later prompt");
    crate::provider::Provider::attach_user_parts(&mut user, vec![part.clone()]).unwrap();
    history.push(user);
    assert_eq!(history[..prefix.len()], prefix);
    assert_eq!(
        remaining_budget("openai/gpt-6-luna", &history).files,
        50_000_000 - 9
    );
    let mut budget = remaining_budget("codex/gpt-6-luna", &[]);
    assert!(
        native_part(
            &path,
            "report.pdf",
            "application/pdf",
            "codex/gpt-6-luna",
            false,
            &mut budget
        )
        .await
        .unwrap()
        .is_none()
    );
    // Oversized native input stays a usable workspace file; it is not an ingress rejection.
    let big = tokio::fs::File::create(dir.path().join("big.pdf"))
        .await
        .unwrap();
    big.set_len(50_000_000).await.unwrap();
    assert!(
        native_part(
            &dir.path().join("big.pdf"),
            "big.pdf",
            "application/pdf",
            "openai/gpt-6-luna",
            false,
            &mut remaining_budget("openai/gpt-6-luna", &[])
        )
        .await
        .unwrap()
        .is_none()
    );
    let png = dir.path().join("picture.png");
    tokio::fs::write(&png, b"png bytes").await.unwrap();
    let (_text, parts) = files
        .inspect(
            &png,
            "anthropic/test",
            None,
            remaining_budget("anthropic/test", &[]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(parts[0]["text"].as_str().unwrap().contains("picture.png"));
    assert_eq!(parts[1]["source"]["media_type"], "image/png");
    assert_eq!(remaining_budget("openai/test", &[]).payload, 384_000_000);
    // Binary bytes do not consume the curator's text guard.
    assert!(text_chars(&part) < 100);
}
#[tokio::test]
async fn cleanup_protects_consumers_and_outbox_but_preserves_edits_and_keeps() {
    let (dir, store, files) = fixture();
    let source = dir.path().join("source.txt");
    tokio::fs::write(&source, b"original").await.unwrap();
    let first = files.snapshot(10, "first", &source).await.unwrap();
    let first_copy = files.materialize(&first).await.unwrap();
    files
        .db
        .lock()
        .unwrap()
        .execute("UPDATE attachments SET last_used=0", [])
        .unwrap();
    files.clean(&HashSet::from([10])).await.unwrap();
    assert!(files.blob(&first).exists());
    assert!(first_copy.exists());
    // Curator/reviewer protection is per channel, rechecked transactionally by GC.
    files.protect_curators(&HashSet::from([10])).unwrap();
    files.clean(&HashSet::new()).await.unwrap();
    assert!(first_copy.exists());
    assert!(files.blob(&first).exists());
    files.protect_curators(&HashSet::new()).unwrap();
    let modified = files.snapshot(11, "modified", &source).await.unwrap();
    let modified_copy = files.materialize(&modified).await.unwrap();
    tokio::fs::write(&modified_copy, b"edited").await.unwrap();
    let kept = files.snapshot(12, "kept", &source).await.unwrap();
    files.keep(12, "kept", true).unwrap();
    let queued = files.snapshot(13, "queued", &source).await.unwrap();
    store
        .enqueue_file("send", 13, 2, "caption", "queued", Some(100))
        .unwrap();
    files
        .db
        .lock()
        .unwrap()
        .execute("UPDATE attachments SET last_used=0", [])
        .unwrap();
    files.clean(&HashSet::new()).await.unwrap();
    assert!(!first_copy.exists());
    assert!(!files.blob(&first).exists());
    assert_eq!(tokio::fs::read(&modified_copy).await.unwrap(), b"edited");
    assert!(files.blob(&kept).exists());
    assert!(files.blob(&queued).exists());
    store.sent("send", "999").unwrap();
    files.clean(&HashSet::new()).await.unwrap();
    assert!(!files.blob(&queued).exists());
    assert!(source.exists());
    assert!(files.get(13, "queued").is_ok());
    assert!(files.get(99, "kept").is_err());
}
#[tokio::test]
async fn outbound_snapshot_survives_restart_and_source_mutation() {
    let (dir, store, files) = fixture();
    let path = dir.path().join("result.txt");
    tokio::fs::write(&path, b"frozen").await.unwrap();
    let file = files.snapshot(10, "snapshot", &path).await.unwrap();
    store
        .enqueue_file("delivery", 10, 2, "", "snapshot", Some(100))
        .unwrap();
    tokio::fs::write(&path, b"replacement").await.unwrap();
    drop(files);
    drop(store);
    let store = Store::open(&dir.path().join("runtime.sqlite")).unwrap();
    let files = Attachments::open(dir.path(), dir.path(), AttachmentConfig::default()).unwrap();
    let pending = store.next_outbound().unwrap().unwrap();
    assert_eq!(pending.attachment.as_deref(), Some("snapshot"));
    assert_eq!(pending.reply_to, Some(100));
    assert_eq!(
        tokio::fs::read(files.outgoing_path(&file)).await.unwrap(),
        b"frozen"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn cleanup_never_follows_replaced_workspace_directories() {
    let (dir, _store, files) = fixture();
    let source = dir.path().join("source.txt");
    tokio::fs::write(&source, b"original").await.unwrap();
    let file = files.snapshot(10, "snapshot", &source).await.unwrap();
    let copy = files.materialize(&file).await.unwrap();
    let parent = copy.parent().unwrap();
    tokio::fs::remove_file(&copy).await.unwrap();
    tokio::fs::remove_dir(parent).await.unwrap();
    let outside = dir.path().join("project");
    tokio::fs::create_dir(&outside).await.unwrap();
    tokio::fs::copy(&source, outside.join("source.txt"))
        .await
        .unwrap();
    std::os::unix::fs::symlink(&outside, parent).unwrap();
    files.clean(&HashSet::new()).await.unwrap();
    assert!(outside.join("source.txt").exists());
    assert!(files.materialize(&file).await.is_err());
}
#[tokio::test]
async fn interruption_cleans_partial_download_and_replays_pending_input() {
    let (dir, _store, files) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let notice = started.clone();
    let app = Router::new().route(
        "/slow",
        get(move || {
            let notice = notice.clone();
            async move {
                notice.notify_one();
                std::future::pending::<StatusCode>().await
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let file = IncomingFile {
        id: "200".into(),
        filename: "file.txt".into(),
        content_type: None,
        size: 10,
        url: format!("http://{address}/slow"),
    };
    files
        .queue(
            &Input {
                id: "100".into(),
                channel: 10,
                user: 2,
                text: "".into(),
            },
            &[file],
        )
        .unwrap();
    let pending = files.next(&HashSet::new()).unwrap().unwrap();
    let discord = Discord::new("private".into(), 1, vec![2]).unwrap();
    let cancel = CancellationToken::new();
    let task = {
        let files = files.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { files.receive(&pending, &discord, &cancel).await })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        std::fs::read_dir(dir.path().join("attachments/tmp"))
            .unwrap()
            .count(),
        0
    );
    assert!(files.pending("100").unwrap());
    server.abort();
}
#[tokio::test]
async fn bundled_pdf_fallback_renders_only_requested_page_and_leaves_no_scratch() {
    // Nix checks provide poppler; source-only environments may have no document tools installed.
    if std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_err()
    {
        return;
    }
    let (dir, _store, files) = fixture();
    let path = dir.path().join("two-pages.pdf");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        "<< /Length 39 >>\nstream\nBT /F1 16 Tf 20 240 Td (FIRST PAGE) Tj ET\nendstream",
        "<< /Length 40 >>\nstream\nBT /F1 16 Tf 20 240 Td (SECOND PAGE) Tj ET\nendstream",
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    tokio::fs::write(&path, pdf).await.unwrap();
    let (text, parts) = files
        .inspect(
            &path,
            "codex/gpt-6-luna",
            Some(2),
            remaining_budget("codex/gpt-6-luna", &[]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(text.contains("SECOND PAGE"));
    assert!(!text.contains("FIRST PAGE"));
    assert!(parts[0]["text"].as_str().unwrap().contains("page 2"));
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(
        std::fs::read_dir(files.root.join("tmp")).unwrap().count(),
        0
    );
    let (text, parts) = files
        .inspect(
            &path,
            "openai/gpt-6-luna",
            None,
            remaining_budget("openai/gpt-6-luna", &[]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(text.contains("native file"));
    assert_eq!(parts[0]["type"], "input_file");
}

#[tokio::test]
async fn static_gif_is_native_but_animated_gif_stays_available_without_invalid_provider_input() {
    let (dir, _store, _files) = fixture();
    let single = base64::engine::general_purpose::STANDARD
        .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
        .unwrap();
    assert!(static_gif(&single));
    let mut animated = single[..single.len() - 1].to_vec();
    animated.extend_from_slice(&single[19..single.len() - 1]);
    animated.push(0x3b);
    assert!(!static_gif(&animated));
    assert!(!static_gif(b"GIF89a"));
    let path = dir.path().join("picture.gif");
    tokio::fs::write(&path, &single).await.unwrap();
    assert!(
        native_part(
            &path,
            "picture.gif",
            "image/gif",
            "openai/test",
            false,
            &mut remaining_budget("openai/test", &[])
        )
        .await
        .unwrap()
        .is_some()
    );
    tokio::fs::write(&path, &animated).await.unwrap();
    assert!(
        native_part(
            &path,
            "picture.gif",
            "image/gif",
            "openai/test",
            false,
            &mut remaining_budget("openai/test", &[])
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(path.exists());
}
