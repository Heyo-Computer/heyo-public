import importlib.util
import io
import json
import os
import pathlib
import stat
import tarfile
import tempfile
import unittest

SOURCE = pathlib.Path(__file__).parents[1] / "src" / "host_heyvm_bootstrap.py"
spec = importlib.util.spec_from_file_location("bootstrap", SOURCE)
b = importlib.util.module_from_spec(spec); spec.loader.exec_module(b)


def tar(entries, gz=True):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz" if gz else "w") as archive:
        for name, data, kind in entries:
            info = tarfile.TarInfo(name); info.size = len(data)
            if kind == "file": archive.addfile(info, io.BytesIO(data))
            elif kind == "link": info.type = tarfile.SYMTYPE; info.linkname = "heyvm"; archive.addfile(info)
    return out.getvalue()


class Response(io.BytesIO):
    status = 200
    def __enter__(self): return self
    def __exit__(self, *_): pass


class Opener:
    def __init__(self, data, status=200): self.data=data; self.status=status
    def open(self, *_args, **_kwargs): r=Response(self.data); r.status=self.status; return r


class FakeHost(b.Host):
    def __init__(self, target, old, new, fail=None, rollback_fail=False):
        self.target=target; self.old=old; self.new=new; self.current=old; self.pid=10; self.start="1"; self.fail=fail; self.rollback_fail=rollback_fail; self.calls=[]
    def command(self, argv):
        self.calls.append(tuple(argv))
        action = argv[1] if len(argv)>1 else ""
        if self.fail == action: self.fail=None; raise RuntimeError(action)
        if action == "restart":
            disk=pathlib.Path(self.target["executable"]).read_bytes()
            if self.rollback_fail and disk == self.old: self.current=self.new
            else: self.current=disk
            self.pid += 1; self.start=str(int(self.start)+1)
        return "LoadState=loaded\nActiveState=active\nKillMode=process\nMainPID=%s\nExecStart={ path=%s ; argv[]=/misleading/heyvm --serve ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n" % (self.pid,self.target["executable"])
    def boot_id(self): return "boot"
    def starttime(self, _pid): return self.start
    def proc_exe(self, _pid): return os.path.realpath(self.target["executable"])
    def proc_digest(self, _pid): return b.sha(self.current)
    def environment_has(self, _pid, _expected):
        if self.fail == "environment": self.fail=None; return False
        return True
    def health(self, _url):
        if self.fail == "health": self.fail=None; raise RuntimeError("health")
        return {"status":"healthy","backendId":self.target["backend_server_id"],"backendRegion":self.target["region"]}


class Tests(unittest.TestCase):
    def target(self, root):
        return {"repository":b.REPOSITORY,"app_lb_admin_url":"https://admin.example","app_lb_deployment":"host",
          "app_lb_namespace":"default","runner_hd_id":"runner","backend_server_id":"backend","executable":str(root/"bin/heyvm"),
          "unit":"heyvmd.service","state_dir":str(root/"state"),"config_json_path":str(root/"etc/config.json"),
          "systemd_drop_in_path":str(root/"etc/systemd/heyvmd.service.d/update.conf"),"local_health_url":"http://127.0.0.1:8080/health",
          "target_alias":"eu1","region":"eu1"}
    def req(self, binary=b"\x7fELFnew"):
        return {"operation_id":"op-1","artifact_url":"https://art.example/blob","artifact_sha256":"a"*64,"artifact_size":1,
          "inner_path":"validation/heyvm.tar.gz","inner_archive_sha256":"b"*64,"heyvm_sha256":b.sha(binary)}

    def test_mapping_closed_and_safe(self):
        with tempfile.TemporaryDirectory() as td:
            target=self.target(pathlib.Path(td)); raw=json.dumps({"eu1":target})
            self.assertEqual(b.mapping(raw,"eu1"),target)
            for edit in ({"extra":"x"},{"repository":"public"},{"unit":"heyvmd"},{"executable":"../bad"},{"app_lb_admin_url":"http://admin"},{"local_health_url":"http://example/health"}):
                bad=target.copy(); bad.update(edit)
                with self.assertRaises(ValueError): b.mapping(json.dumps({"eu1":bad}),"eu1")
            with self.assertRaises(ValueError): b.mapping(raw,"unknown")

    def test_download_size_hash_status_and_no_redirect_handler(self):
        req=self.req(); data=b"x"; req.update(artifact_size=1,artifact_sha256=b.sha(data))
        self.assertEqual(b.download(req,Opener(data)),data)
        for payload,status in [(b"xx",200),(b"x",302),(b"y",200)]:
            with self.assertRaises(ValueError): b.download(req,Opener(payload,status))
        self.assertIsNone(b.NoRedirect().redirect_request(None,None,None,None,None,None))

    def test_service_uses_authoritative_systemd_path_not_argv(self):
        with tempfile.TemporaryDirectory() as td:
            target=self.target(pathlib.Path(td)); exe=pathlib.Path(target["executable"]); exe.parent.mkdir(); exe.write_bytes(b"old")
            host=FakeHost(target,b"old",b"new")
            self.assertEqual(b.service(host,target)["pid"],10)
            host.command=lambda _argv: "LoadState=loaded\nActiveState=active\nKillMode=process\nMainPID=10\nExecStart={ path=/wrong/heyvm ; argv[]=%s ; }\n" % target["executable"]
            with self.assertRaises(ValueError): b.service(host,target)

    def test_legacy_launcher_symlink_is_verified_and_restored_exactly(self):
        with tempfile.TemporaryDirectory() as td:
            root=pathlib.Path(td); target=self.target(root); old=b"\x7fELFold"; new=b"\x7fELFnew"
            release=root/"releases/old/heyvm"; release.parent.mkdir(parents=True); release.write_bytes(old); release.chmod(0o755)
            exe=pathlib.Path(target["executable"]); exe.parent.mkdir(); exe.symlink_to(release)
            host=FakeHost(target,old,new)
            host.proc_exe=lambda _pid: os.path.realpath(target["executable"])
            self.assertEqual(b.service(host,target)["disk_sha256"],b.sha(old))
            exe.unlink(); exe.write_bytes(new); exe.chmod(0o755); host.current=new
            journal={"service":{"disk_sha256":b.sha(old),"running_sha256":b.sha(old)},"predecessors":{
              "executable":{"present":True,"link_target":str(release)},"config":{"present":False},"drop_in":{"present":False}}}
            b.restore(host,target,journal)
            self.assertTrue(exe.is_symlink()); self.assertEqual(os.readlink(exe),str(release)); self.assertEqual(exe.read_bytes(),old)

    def test_archives_reject_traversal_links_ambiguity_and_hashes(self):
        binary=b"\x7fELFpayload"; inner=tar([("heyvm",binary,"file")]); outer=tar([("validation/heyvm.tar.gz",inner,"file")],False)
        req=self.req(binary); req.update(artifact_size=len(outer),artifact_sha256=b.sha(outer),inner_archive_sha256=b.sha(inner))
        self.assertEqual(b.executable(outer,req),binary)
        cases=[tar([("../escape",b"x","file"),("validation/heyvm.tar.gz",inner,"file")],False),
               tar([("validation/heyvm.tar.gz",b"","link")],False),
               tar([("validation/heyvm.tar.gz",inner,"file"),("validation/heyvm.tar.gz",inner,"file")],False)]
        for value in cases:
            with self.assertRaises(ValueError): b.executable(value,req)
        linked=tar([("heyvm",b"","link")]); out=tar([("validation/heyvm.tar.gz",linked,"file")],False); bad=req.copy(); bad["inner_archive_sha256"]=b.sha(linked)
        with self.assertRaises(ValueError): b.executable(out,bad)

    def run_install(self, fail=None, rollback_fail=False):
        temp=tempfile.TemporaryDirectory(); root=pathlib.Path(temp.name); target=self.target(root)
        old=b"\x7fELFold"; new=b"\x7fELFnew"; exe=pathlib.Path(target["executable"]); exe.parent.mkdir(); exe.write_bytes(old); exe.chmod(0o755)
        original=b.secure_file
        def test_secure(path,limit,**_options):
            p=pathlib.Path(path)
            if not p.exists(): return {"present":False}
            return {"present":True,"mode":stat.S_IMODE(p.stat().st_mode),"bytes":__import__('base64').b64encode(p.read_bytes()).decode()}
        b.secure_file=test_secure
        original_regular=b.exact_regular; b.exact_regular=lambda path,mode: stat.S_IMODE(pathlib.Path(path).stat().st_mode) == mode
        host=FakeHost(target,old,new,fail,rollback_fail)
        try: result=b.install(target,self.req(new),new,host)
        finally: b.secure_file=original; b.exact_regular=original_regular
        return temp,target,host,result,old,new

    def test_success_replay_conflict_and_predecessor_drift(self):
        temp,target,host,result,old,new=self.run_install()
        try:
            self.assertEqual(result["status"],"succeeded"); self.assertEqual(pathlib.Path(target["executable"]).read_bytes(),new)
            self.assertEqual(b.install(target,self.req(new),new,host),result)
            changed=self.req(new); changed["artifact_size"]=2
            with self.assertRaises(ValueError): b.install(target,changed,new,host)
        finally: temp.cleanup()
        temp,target,host,_,old,new=self.run_install(); temp.cleanup()
        with tempfile.TemporaryDirectory() as td:
            root=pathlib.Path(td); target=self.target(root); p=pathlib.Path(target["executable"]); p.parent.mkdir(); p.write_bytes(old)
            host=FakeHost(target,old,new); host.current=b"different"
            with self.assertRaises(ValueError): b.install(target,self.req(new),new,host)

    def test_recovery_verifies_live_state_without_mutations(self):
        from unittest.mock import patch
        for drift in (None, "running", "config", "drop", "journal", "health", "environment", "mode"):
            temp,target,host,result,old,new=self.run_install()
            try:
                if drift == "running": host.current=old
                if drift in ("config", "drop"):
                    pathlib.Path(target["config_json_path" if drift == "config" else "systemd_drop_in_path"]).write_bytes(b"drift")
                if drift == "journal":
                    p=pathlib.Path(target["state_dir"])/"op-1.json"; saved=json.loads(p.read_bytes()); saved["request_sha256"]="0"*64; p.write_text(json.dumps(saved))
                if drift == "health": host.health=lambda _: {"status":"healthy","backendId":"wrong","backendRegion":"eu1"}
                if drift == "environment": host.fail="environment"
                if drift == "mode": pathlib.Path(target["executable"]).chmod(0o777)
                files={p:p.read_bytes() for p in pathlib.Path(temp.name).rglob("*") if p.is_file()}
                host.calls.clear()
                with patch.object(b,"exact_regular",side_effect=lambda path,mode: stat.S_IMODE(pathlib.Path(path).stat().st_mode)==mode), \
                     patch.object(b,"atomic",side_effect=AssertionError("verification wrote a file")), \
                     patch.object(b,"download",side_effect=AssertionError("verification downloaded an artifact")):
                    if drift:
                        with self.assertRaises(ValueError): b.verify_existing(target,self.req(new),host)
                    else:
                        self.assertEqual(b.verify_existing(target,self.req(new),host),result)
                self.assertTrue(all(call[:2] == ("systemctl","show") for call in host.calls))
                self.assertEqual({p:p.read_bytes() for p in files},files)
            finally: temp.cleanup()

    def test_all_mutation_boundaries_roll_back_and_rollback_failure_is_retained(self):
        for failure in ("daemon-reload","restart","health","environment"):
            temp,target,_host,result,old,_new=self.run_install(failure)
            try:
                self.assertEqual(result["status"],"rolled_back",failure); self.assertEqual(pathlib.Path(target["executable"]).read_bytes(),old)
                journal=json.loads((pathlib.Path(target["state_dir"])/"op-1.json").read_text())
                self.assertEqual(journal["failure"]["type"],"ValueError" if failure=="environment" else "RuntimeError")
                line=SOURCE.read_text().splitlines()[journal["failure"]["line"]-1]
                self.assertIn({"daemon-reload":"daemon-reload", "restart":"restart", "health":"wait_for_health", "environment":"host.environment_has"}[failure],line)
                self.assertNotIn("rollback_failure",journal)
                self.assertEqual(journal["result"],result)
            finally: temp.cleanup()
        # Atomic write failures at each of the three writes.
        for index in range(3):
            temp=tempfile.TemporaryDirectory(); root=pathlib.Path(temp.name); target=self.target(root); old=b"\x7fELFold"; new=b"\x7fELFnew"
            p=pathlib.Path(target["executable"]); p.parent.mkdir(); p.write_bytes(old); host=FakeHost(target,old,new)
            original_secure,original_atomic,original_regular=b.secure_file,b.atomic,b.exact_regular; count=[0]
            b.secure_file=lambda path,limit,**_options: ({"present":True,"mode":0o644,"bytes":__import__('base64').b64encode(pathlib.Path(path).read_bytes()).decode()} if pathlib.Path(path).exists() else {"present":False})
            b.exact_regular=lambda path,mode: stat.S_IMODE(pathlib.Path(path).stat().st_mode) == mode
            def failing(path,data,mode):
                count[0]+=1
                if count[0] == index+2: raise RuntimeError("write") # journal is write one
                return original_atomic(path,data,mode)
            b.atomic=failing
            try: result=b.install(target,self.req(new),new,host); self.assertEqual(result["status"],"rolled_back")
            finally: b.secure_file=original_secure; b.atomic=original_atomic; b.exact_regular=original_regular; temp.cleanup()
        temp,target,host,result,_old,_new=self.run_install("health",True)
        try:
            self.assertEqual(result["status"],"rollback_failed")
            journal=json.loads((pathlib.Path(target["state_dir"])/"op-1.json").read_text())
            self.assertEqual(journal["failure"]["type"],"RuntimeError")
            self.assertEqual(journal["rollback_failure"]["type"],"ValueError")
            self.assertNotEqual(journal["failure"]["line"],journal["rollback_failure"]["line"])
            self.assertEqual(b.install(target,self.req(),b"\x7fELFnew",host)["status"],"rollback_failed")
            self.assertEqual(json.loads((pathlib.Path(target["state_dir"])/"op-1.json").read_text()),journal)
        finally: temp.cleanup()


    def test_listener_startup_wait_is_bounded_and_does_not_hide_bad_identity(self):
        from unittest.mock import patch
        import urllib.error
        original=FakeHost.health
        attempts=[]
        def starting(host,url):
            attempts.append(url)
            if len(attempts)==1: raise urllib.error.URLError("listener not ready")
            if len(attempts)==2: raise urllib.error.HTTPError(url,503,"starting",{},io.BytesIO())
            return original(host,url)
        with patch.object(FakeHost,"health",starting), patch.object(b.time,"sleep"):
            temp,target,host,result,old,new=self.run_install()
        try:
            self.assertEqual(result["status"],"succeeded")
            self.assertEqual(len(attempts),3)
        finally: temp.cleanup()
        for failure in (urllib.error.URLError("still starting"), urllib.error.HTTPError("http://localhost",401,"denied",{},io.BytesIO())):
            with patch.object(FakeHost,"health",side_effect=failure) as health, patch.object(b.time,"monotonic",side_effect=[0,31]), patch.object(b.time,"sleep") as sleep:
                temp,target,host,result,old,new=self.run_install()
            try:
                self.assertEqual(result["status"],"rolled_back")
                self.assertEqual(health.call_count,1)
                sleep.assert_not_called()
            finally: temp.cleanup()
        with patch.object(FakeHost,"health",return_value={"status":"healthy","backendId":"other","backendRegion":"eu1"}) as health, patch.object(b.time,"sleep") as sleep:
            temp,target,host,result,old,new=self.run_install()
        try:
            self.assertEqual(result["status"],"rolled_back")
            self.assertEqual(health.call_count,1)
            sleep.assert_not_called()
        finally: temp.cleanup()
        def restarted(host,url):
            host.pid += 1
            return original(host,url)
        with patch.object(FakeHost,"health",restarted):
            temp,target,host,result,old,new=self.run_install()
        try:
            self.assertEqual(result["status"],"rolled_back")
            self.assertEqual(pathlib.Path(target["executable"]).read_bytes(),old)
        finally: temp.cleanup()


if __name__ == "__main__": unittest.main()
