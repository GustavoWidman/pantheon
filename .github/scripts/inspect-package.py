"""Hosted static package evidence. Never invokes the packaged program or provider.
The unchanged release recipe inherently runs Cargo tests and Camoufox --version
when those derivations build; these are not separate smoke-test invocations.
"""
import hashlib, json, os, re, selectors, shlex, signal, stat, subprocess, sys, time
from pathlib import Path

COMMIT = 'e358375a94d0f949caa369f20bd537efe98e28af'
TREE = 'b0ba32101b0f7bf1bf50fdc9afd527db5d07c11d'
CI_DRV = '/nix/store/9vpgvfcxjv907kbwx5a3kv7ghjkg5nff-pantheon-0.21.1.drv'
MAX_EVIDENCE = 16 * 1024 * 1024
STORE = re.compile(r'/nix/store/[a-z0-9]{32}-[^/\s\x00\'";:)]+')
R = Path.cwd().resolve()
E = R / 'package-evidence'
B = R / 'release'
TOTAL_HASHED = 0


def need(ok, message):
    if not ok: raise RuntimeError(message)


def sync(p):
    f = os.open(p, os.O_RDONLY | os.O_DIRECTORY)
    try: os.fsync(f)
    finally: os.close(f)


def evidence_bytes():
    total = 0
    entries = list(E.iterdir())
    need(len(entries) <= 100, 'too many evidence entries')
    for p in entries:
        s = p.lstat()
        need(stat.S_ISREG(s.st_mode) and s.st_size <= 8*1024*1024, 'bad evidence entry')
        total += s.st_size
    return total


def save(name, obj):
    data = (json.dumps(obj, indent=2, sort_keys=True) + '\n').encode()
    need(len(data) + evidence_bytes() <= MAX_EVIDENCE, 'evidence cap exceeded')
    fd = os.open(E/name, os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as f:
        f.write(data); f.flush(); os.fsync(f.fileno())
    sync(E)


def disk():
    need(os.statvfs(R).f_bavail * os.statvfs(R).f_frsize >= 5*1024**3,
         'hosted free disk below 5GiB; no retry')


def run(name, argv, timeout=120, log_limit=1024*1024):
    """Bound pipes during production, not after capture. Each command runs once."""
    need(Path(argv[0]).is_absolute() and Path(argv[0]).is_file(), 'absolute tool missing')
    disk()
    save(name+'-intent.json', {'argv':argv, 'timeout_s':timeout, 'no_retry':True})
    p = subprocess.Popen(argv, cwd=B, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         start_new_session=True, env=os.environ.copy())
    sel = selectors.DefaultSelector()
    for f in (p.stdout, p.stderr):
        os.set_blocking(f.fileno(), False); sel.register(f, selectors.EVENT_READ)
    output = {p.stdout:bytearray(), p.stderr:bytearray()}
    deadline = time.monotonic() + timeout
    try:
        while sel.get_map():
            need(time.monotonic() < deadline, name+' timed out; no retry')
            disk()
            for key, _ in sel.select(.25):
                buf = os.read(key.fd, 65536)
                if not buf: sel.unregister(key.fileobj); continue
                limit = 1024*1024 if key.fileobj is p.stdout else log_limit
                need(len(output[key.fileobj])+len(buf) <= limit, name+' output cap exceeded')
                output[key.fileobj].extend(buf)
        rc = p.wait(timeout=max(.1, deadline-time.monotonic()))
    except BaseException:
        try: os.killpg(p.pid, signal.SIGTERM)
        except ProcessLookupError: pass
        try: p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try: os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError: pass
        raise
    finally:
        sel.close(); p.stdout.close(); p.stderr.close()
    result = {'returncode':rc, 'stdout':output[p.stdout].decode('utf-8', 'strict'),
              'stderr':output[p.stderr].decode('utf-8', 'replace')}
    save(name+'-result.json', result)
    need(rc == 0, name+' failed; do not retry')
    return result['stdout'].strip()


def tool(name):
    # Hosted runner tools vary; exact bytes/realpaths are recorded before use.
    for d in os.environ['PATH'].split(os.pathsep):
        p = Path(d)/name
        if p.is_file() and os.access(p, os.X_OK):
            p = p.resolve(); need(p.is_absolute(), 'bad tool')
            return str(p)
    raise RuntimeError('tool absent: '+name)


def hashfile(p, cap=512*1024*1024):
    global TOTAL_HASHED
    st = p.lstat()
    need(stat.S_ISREG(st.st_mode) and st.st_size <= cap, 'file type/size bound: '+str(p))
    need(TOTAL_HASHED+st.st_size <= 1024**3, 'total read budget')
    h = hashlib.sha256()
    with p.open('rb') as f:
        left = st.st_size
        while left:
            chunk = f.read(min(left, 1024*1024)); need(chunk, 'truncated read')
            h.update(chunk); left -= len(chunk)
        need(not f.read(1), 'file grew during read')
    TOTAL_HASHED += st.st_size
    after=p.lstat()
    need((after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_mode)==(st.st_dev,st.st_ino,st.st_size,st.st_mtime_ns,st.st_mode), 'file metadata changed')
    return h.hexdigest()


def small(p, limit=1024*1024):
    s = p.lstat(); need(stat.S_ISREG(s.st_mode) and s.st_size <= limit, 'bounded regular read')
    with p.open('rb') as f: data = f.read(limit+1)
    need(len(data) == s.st_size and len(data) <= limit, 'read changed')
    return data


def storepath(value):
    need(isinstance(value, str) and STORE.fullmatch(value), 'invalid store root')
    return Path(value)


def deref(p, roots):
    # Component-wise symlink handling, at most 16 redirects; never traverses nonstore.
    p = Path(os.path.normpath(str(p)))
    for _ in range(16):
        need(any(p == r or r in p.parents for r in roots), 'outside allowed roots: '+str(p))
        parts = p.parts; redirect = None
        for i in range(3, len(parts)+1):
            q = Path(*parts[:i]); s = q.lstat()
            if stat.S_ISLNK(s.st_mode):
                t = os.readlink(q)
                need(len(t) <= 4096, 'long link')
                dest = Path(t) if os.path.isabs(t) else q.parent/t
                redirect = Path(os.path.normpath(str(dest.joinpath(*parts[i:])))); break
        if redirect is None: return p
        p = redirect
    raise RuntimeError('symlink hop limit')


def manifest(roots):
    rows = []
    for root in roots:
        stack = [root]
        while stack:
            p = stack.pop(); s = p.lstat()
            need(len(rows) < 10000, 'installed entry limit')
            row = {'output':str(root), 'path':str(p.relative_to(root)), 'mode':oct(stat.S_IMODE(s.st_mode))}
            if stat.S_ISLNK(s.st_mode):
                target = os.readlink(p); need(len(target) <= 4096, 'long link')
                row.update(type='symlink', target=target)
                # Record links, do not recursively follow dependency trees.
                d = Path(os.path.normpath(str(Path(target) if os.path.isabs(target) else p.parent/target)))
                need(str(d).startswith('/nix/store/'), 'nonstore installed link')
            elif stat.S_ISDIR(s.st_mode):
                row['type'] = 'directory'
                with os.scandir(p) as it:
                    for x in it:
                        need(len(stack)+len(rows) < 10000, 'directory width limit')
                        stack.append(Path(x.path))
            elif stat.S_ISREG(s.st_mode):
                row.update(type='regular', bytes=s.st_size, sha256=hashfile(p))
            else: raise RuntimeError('unexpected installed type')
            rows.append(row)
    return rows


def drv(name, path, nix):
    storepath(path); need(path.endswith('.drv'), 'not derivation')
    obj = json.loads(run(name, [nix,'derivation','show',path]))
    need(isinstance(obj,dict) and set(obj) == {path}, 'ambiguous derivation')
    value = obj[path]
    need(value['system'] == 'x86_64-linux', 'wrong system')
    return value


def out(d):
    need(set(d['outputs']) == {'out'}, 'multiple outputs')
    return storepath(d['outputs']['out']['path'])


def inspect():
    need(sys.argv == [sys.argv[0], 'inspect'], 'one inspect mode')
    need(os.environ.get('GITHUB_RUN_ATTEMPT') == '1', 'job rerun forbidden')
    need(not os.path.lexists(E), 'evidence namespace consumed')
    E.mkdir(mode=0o700); sync(R)
    save('operation-intent.json', {'commit':COMMIT,'tree':TREE,'scope':'one newly reviewed hosted realization and static inspection','ARM':'SKIPPED','reproducibility':False})
    tools = {n:tool(n) for n in ('git','nix')}
    save('hosted-tools.json', {n:{'path':p,'sha256':hashfile(Path(p))} for n,p in tools.items()})
    g, n = tools['git'], tools['nix']
    need(run('release-head',[g,'rev-parse','HEAD']) == COMMIT, 'release commit mismatch')
    need(run('release-tree',[g,'rev-parse','HEAD^{tree}']) == TREE, 'release tree mismatch')
    need(run('release-status',[g,'status','--porcelain','--untracked-files=all']) == '', 'release dirty')
    need(os.statvfs(R).f_bavail*os.statvfs(R).f_frsize >= 20*1024**3, 'need 20GiB before build')
    build = json.loads(run('build',[n,'build','--no-link','--json','--print-build-logs','--no-write-lock-file',
        '--builders','','--max-jobs','2','--cores','2','git+file://'+str(B)+'?rev='+COMMIT+'&shallow=1#packages.x86_64-linux.pantheon'],timeout=4800,log_limit=4*1024*1024))
    need(isinstance(build,list) and len(build)==1 and set(build[0]['outputs'])=={'out'}, 'ambiguous build output')
    w = storepath(build[0]['outputs']['out']); wd = build[0]['drvPath']
    d = drv('wrapper-drv',wd,n); need(out(d)==w, 'wrapper output mismatch')
    # Save mismatch distinctly and stop: source equality is not old-CI-artifact equality.
    save('derivation-comparison.json', {'observed':wd,'saved_CI':CI_DRV,'matches':wd==CI_DRV,
        'claim':'same derivation only, not reproducible-build equality'})
    need(wd == CI_DRV, 'derivation mismatch; source build retained, same-CI-artifact claim blocked')
    deps = d['inputDrvs']; need(isinstance(deps,dict) and len(deps)<=100, 'input derivation limit')
    def one(suffix):
        xs = [x for x in deps if x.endswith(suffix)]
        need(len(xs)==1, 'dependency ambiguous: '+suffix); return xs[0]
    ud = one('-pantheon-unwrapped-0.21.1.drv')
    bd = one('-pantheon-browser-runtime-156.0.1-beta.34.drv')
    u = out(drv('unwrapped-drv',ud,n)); browser_d = drv('browser-drv',bd,n); b = out(browser_d)
    need(d['env']['paths'].split() == [str(u)], 'symlinkJoin source mismatch')
    command = d['env']['buildCommand']; need(isinstance(command,str) and len(command)<1024*1024, 'build command bound')
    roots = {storepath(x) for x in STORE.findall(command)} | {w,u,b}
    need(len(roots) <= 64, 'wrapper dependency limit')
    lines = small(w/'bin/pantheon',128*1024).decode().splitlines()
    need(lines and re.fullmatch(r'#! /nix/store/[a-z0-9]{32}-bash-[^ /]+/bin/bash -e',lines[0]), 'not expected shell wrapper')
    exports = {}
    for line in lines:
        m = re.fullmatch(r'export ([A-Z0-9_]+)=(.*)',line)
        if m: need(m[1] not in exports,'duplicate export'); exports[m[1]]=m[2]
    keys = ['SSL_CERT_FILE','PANTHEON_CODEX_CLI','PANTHEON_PDFTOTEXT','PANTHEON_PDFTOPPM',
        'PANTHEON_BROWSER_PYTHON','PANTHEON_NOVNC_WEB','PANTHEON_CAMOUFOX','PANTHEON_XVFB',
        'PANTHEON_X11VNC','PANTHEON_BROWSER_WORKER']
    need(set(exports)==set(keys), 'unexpected wrapper export set')
    expected = {}
    for k in keys:
        if k == 'PANTHEON_BROWSER_WORKER': target = str(w/'libexec/pantheon/browser-worker.py')
        else:
            m = re.search(r'--set(?:-default)? '+k+r' "([^"\n]+)"',command)
            need(m is not None, 'recipe env missing: '+k); target = m[1]
        if k=='SSL_CERT_FILE':
            need(exports[k] in ('${SSL_CERT_FILE-\''+target+'\'}','${SSL_CERT_FILE-"'+target+'"}'), 'default export mismatch')
        else: need(exports[k] in ("'"+target+"'",'"'+target+'"',target), 'export mismatch: '+k)
        expected[k] = target
    m = re.search(r'--prefix PATH : "([^"\n]+)"',command)
    need(m is not None, 'recipe PATH absent')
    path_expected = m[1].split(':'); need(1<=len(path_expected)<=16 and len(set(path_expected))==len(path_expected), 'PATH bound')
    path_actual = []
    for line in lines:
        if line.startswith("PATH='"):
            p = line.split("'",2)[1]
            if p.startswith('/nix/store/'): path_actual.append(p)
    # makeWrapper prepends entries in reverse textual order to preserve final prefix order.
    need(path_actual == list(reversed(path_expected)), 'PATH construction differs')
    wrapped = w/'bin/.pantheon-wrapped'
    ex = [line for line in lines if line.startswith('exec ')]
    need(len(ex)==1 and shlex.split(ex[0]) == ['exec','-a','$0',str(wrapped),'$@'], 'wrapper exec differs')
    # Compare the entire generated program, not only its interesting exports.
    program = [lines[0]]
    for k in keys[:-1]:
        value = expected[k]
        program.append('export '+k+'='+('${SSL_CERT_FILE-\''+value+'\'}' if k=='SSL_CERT_FILE' else "'"+value+"'"))
    for v in reversed(path_expected):
        program.extend(["PATH=${PATH:+':'$PATH':'}","PATH=${PATH/':''"+v+"'':'/':'}","PATH='"+v+"'$PATH","PATH=${PATH#':'}","PATH=${PATH%':'}",'export PATH'])
    program.extend(["export PANTHEON_BROWSER_WORKER='"+expected['PANTHEON_BROWSER_WORKER']+"'",ex[0]])
    need(lines == program, 'wrapper contains unexpected program statements')
    actual = deref(wrapped,{w,u}); need(actual == u/'bin/pantheon', 'wrapped target outside declared binary')
    elf = small_header(actual)
    need(elf[:4]==b'\x7fELF' and elf[4:6]==b'\x02\x01' and int.from_bytes(elf[18:20],'little')==62, 'not x86_64 ELF')
    matches = []
    pairs = [('libexec/pantheon/browser-worker.py','scripts/browser-worker.py',w),
        ('share/licenses/pantheon/LICENSE','LICENSE',u),
        ('share/licenses/pantheon/THIRD_PARTY_NOTICES.md','THIRD_PARTY_NOTICES.md',u),
        ('share/licenses/pantheon/uniichat-Apache-2.0.txt','licenses/uniichat-Apache-2.0.txt',u),
        ('share/licenses/pantheon/rmcp-Apache-2.0.txt','vendor/rmcp/LICENSE',u)]
    for rel,src,root in pairs:
        installed = deref(root/rel,{w,u}); source=B/src
        need(small(installed)==small(source),'source bytes mismatch: '+src)
        matches.append({'installed':str(installed),'source':src,'sha256':hashfile(installed)})
    # Require the expected browser paths exist; no browser-dependency tree traversal.
    browser_paths = []
    for rel in ('bin/pantheon-browser-python','bin/Xvfb','bin/x11vnc','share/novnc','libexec/camoufox/camoufox-bin'):
        p = b/rel; s=p.lstat()
        if rel in ('bin/Xvfb','bin/x11vnc','share/novnc'):
            phase = browser_d['env']['installPhase']
            m = re.search(r'ln -s ([^ \n]+) \"\$out/'+re.escape(rel)+r'\"',phase)
            need(m is not None and stat.S_ISLNK(s.st_mode) and os.readlink(p)==m[1], 'browser recipe link differs')
            need(STORE.match(m[1]), 'browser dependency not store')
        else:
            need(stat.S_ISREG(s.st_mode) and s.st_mode&0o111, 'browser executable type/mode')
        browser_paths.append({'path':str(p),'mode':oct(stat.S_IMODE(s.st_mode)),'link':os.readlink(p) if p.is_symlink() else None})
    info = json.loads(run('output-metadata',[n,'path-info','--offline','--json',str(w),str(u),str(b)]))
    if isinstance(info,list): info={x['path']:x for x in info}
    need(isinstance(info,dict) and set(info)=={str(w),str(u),str(b)}, 'output metadata differs')
    for path,obj in info.items():
        need(isinstance(obj.get('narHash'),str) and obj['narHash'].startswith('sha256-'),'NAR hash missing')
        refs=obj.get('references'); need(isinstance(refs,list) and len(refs)<=256,'reference list missing/big')
        for x in refs: storepath(x)
    need(str(u) in info[str(w)]['references'] and str(b) in info[str(w)]['references'], 'wrapper registered deps missing')
    for target in list(expected.values())+path_expected+[lines[0].split()[1]]:
        root = STORE.match(target); need(root is not None, 'dependency path not store')
        need(root[0] == str(w) or root[0] in info[str(w)]['references'], 'wrapper reference unregistered')
    save('installed-manifest.json',manifest([w,u]))
    save('static-checks.json',{'release_commit':COMMIT,'release_tree':TREE,'wrapper':str(w),'unwrapped':str(u),'browser':str(b),
        'binary_sha256':hashfile(actual),'wrapper_sha256':hashfile(w/'bin/pantheon'),'ELF':'64-bit little-endian x86_64',
        'source_matches':matches,'wrapper_environment':expected,'PATH_prefix':path_expected,'browser_selected_entries':browser_paths,
        'output_metadata':info,'runtime_verified':False,'reproducibility_verified':False,'ARM':'SKIPPED'})
    save('complete.json',{'status':'STATIC_X86_64_PACKAGE_CONTENTS_OBSERVED_REQUIRES_INDEPENDENT_ACCEPTANCE','tree':TREE,'commit':COMMIT})


def small_header(p):
    need(stat.S_ISREG(p.lstat().st_mode), 'ELF not regular')
    with p.open('rb') as f: return f.read(64)


def seal():
    # Always-run, evidence-only stage. Never include binaries or inherited environment.
    if not E.exists(): E.mkdir(mode=0o700); sync(R)
    need(E.is_dir() and not E.is_symlink(), 'evidence root')
    total=evidence_bytes(); need(total<=MAX_EVIDENCE,'evidence too large for upload')
    save('seal.json',{'bytes_before_seal':total,'limit':MAX_EVIDENCE,'inspection_complete':(E/'complete.json').is_file(),
        'note':'intrinsic Nix recipe tests/browser fixup may execute during build; no separate package invocation'})


if __name__=='__main__':
    try:
        if len(sys.argv)==2 and sys.argv[1]=='seal': seal()
        else: inspect()
    except Exception as e:
        print('STOP_NO_RETRY: '+str(e),file=sys.stderr)
        sys.exit(1)
