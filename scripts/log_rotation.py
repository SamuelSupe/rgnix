#!/usr/bin/env python3
"""Local log rotation acceptance: HTTP requests, actual files, signals and logrotate."""
import concurrent.futures
import contextlib
import gzip
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from integration import RESULTS, check, free_port, request, wait_for


class Server:
    def __init__(self, binary, directory, policy="off", shared=False, access=None, error=None):
        self.port, self.admin = free_port(), free_port()
        self.directory = directory
        self.access = access or directory / "access.log"
        self.error = error or (self.access if shared else directory / "error.log")
        self.config = directory / "nginx.conf"
        self.configure(policy)
        self.output = open(directory / "process.log", "w+")
        self.process = subprocess.Popen([binary, "serve", "-c", str(self.config), "--admin", f"127.0.0.1:{self.admin}"],
            stdout=self.output, stderr=self.output, env={k:v for k,v in os.environ.items() if not k.startswith("OTEL_")})

    def configure(self, policy):
        self.config.write_text(f'''events {{}} http {{
            access_log {self.access}; error_log {self.error} warn;
            rgnix_log_rotation {policy};
            server {{ listen 127.0.0.1:{self.port};
                location / {{ return 200 "ok"; }}
                location /fail {{ proxy_pass http://127.0.0.1:1; }}
            }}
        }}''')

    def metric(self, name):
        text = request(self.admin, "/metrics")[2].decode()
        return sum(float(line.rsplit(" ",1)[1]) for line in text.splitlines()
                   if line.startswith(name + " ") or line.startswith(name + "{"))

    def drain(self, marker=None, path=None):
        # The response can arrive before Pingora calls its completion logging hook.
        if marker:
            wait_for(lambda: marker in (path or self.access).read_text())
        wait_for(lambda: self.metric("rgnix_file_logs_pending") == 0)

    def reload(self, policy):
        previous = self.metric("rgnix_config_version")
        self.configure(policy)
        self.process.send_signal(signal.SIGHUP)
        wait_for(lambda: self.metric("rgnix_config_version"), previous + 1)
        self.drain()

    def get(self, path):
        return request(self.port, path)[0]

    def stop(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            self.process.wait(timeout=12)


@contextlib.contextmanager
def running(binary, root, **options):
    directory = root / str(free_port())
    directory.mkdir()
    server = Server(binary, directory, **options)
    try:
        wait_for(lambda: request(server.admin, "/readyz")[0], 200)
        yield server
    except BaseException:
        server.output.flush()
        print((directory / "process.log").read_text()[-4000:], file=sys.stderr)
        raise
    finally:
        try:
            server.stop()
        finally:
            if server.process.poll() is None:
                server.process.kill()
                server.process.wait()
            server.output.close()


def archives(path):
    return sorted(path.parent.glob(path.name + ".rgnix.*"))


def content(path):
    return (gzip.decompress(path.read_bytes()) if path.suffix == ".gz" else path.read_bytes()).decode()


def complete_log(path):
    return "".join(content(p) for p in archives(path)) + path.read_text()


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="rgnix-rotation-") as temporary:
        root = Path(temporary)
        with running(binary, root, policy="size=1k keep=100 gzip=on") as server:
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                statuses = list(pool.map(lambda n: server.get(f"/record-{n:03d}"), range(120)))
            server.drain()
            wait_for(lambda: len(re.findall(r'GET /record-(\d+)', complete_log(server.access))) == 120)
            logs = complete_log(server.access)
            ids = re.findall(r'GET /record-(\d+)', logs)
            check("concurrent size rotation preserves each complete access record once", statuses == [200]*120 and sorted(map(int, ids)) == list(range(120)))
            check("rotated files are valid gzip with the configured size bound", bool(archives(server.access)) and all(
                p.suffix == ".gz" and len(gzip.decompress(p.read_bytes())) <= 1024 for p in archives(server.access)))
            server.access.chmod(0o640)
            server.get("/" + "x" * 1500)
            server.drain("x"*1500)
            check("single oversized records remain intact and file mode is preserved", (server.access.stat().st_mode & 0o777) == 0o640 and "x"*1500 in server.access.read_text())
            unrelated = server.access.with_name("access.log.rgnix.notes")
            unrelated.write_text("keep me")
            server.reload("size=1k keep=2 gzip=on")
            for n in range(25): server.get(f"/retained-{n}")
            server.drain("/retained-24")
            check("retention is bounded after a policy reload without deleting unrelated files", len(archives(server.access)) == 3 and unrelated.read_text() == "keep me"
                  and len([p for p in archives(server.access) if p.suffix == ".gz"]) == 2)
            old_count = server.metric("rgnix_log_rotations_total")
            server.configure("size=1k keep=0")
            server.process.send_signal(signal.SIGHUP)
            wait_for(lambda: server.metric("rgnix_reload_errors_total") > 0)
            for n in range(12): server.get(f"/bad-policy-kept-{n}")
            server.drain("/bad-policy-kept-11")
            check("invalid reload preserves the working rotation policy", server.metric("rgnix_log_rotations_total") > old_count)
            server.reload("off")
            old_count = server.metric("rgnix_log_rotations_total")
            for n in range(12): server.get(f"/rotation-disabled-{n}")
            server.drain("/rotation-disabled-11")
            check("rotation can be disabled by SIGHUP", server.metric("rgnix_log_rotations_total") == old_count)

        with running(binary, root, policy="interval=1s keep=5") as server:
            server.get("/previous-period")
            server.drain("/previous-period")
            time.sleep(1.1)
            server.get("/current-period")
            server.drain("/current-period")
            check("time rotation splits records at the next occupied UTC interval", len(archives(server.access)) == 1
                  and "/previous-period" in content(archives(server.access)[0]) and "/current-period" in server.access.read_text())

        with running(binary, root, policy="size=1k keep=30 gzip=on", shared=True) as server:
            for n in range(15): assert server.get(f"/fail?request={n}") in (502,503)
            server.stop()
            text = complete_log(server.access)
            check("access and error logs sharing one file rotate through the same writer", len(re.findall(r'"GET /fail\?request=', text)) == 15 and text.count("request failed route=") == 15)
            check("shutdown drains queued local log records", server.process.returncode == 0 and text.endswith("\n"))

        with running(binary, root, policy="size=1k keep=5") as server:
            server.get("/before-rotation-error")
            server.drain("/before-rotation-error")
            server.directory.chmod(0o500)
            try:
                server.get("/after-rotation-error/" + "x"*1500)
                server.drain("/after-rotation-error")
                check("rotation failure preserves the active file and reports I/O errors", "/before-rotation-error" in server.access.read_text()
                      and server.metric('rgnix_log_io_errors_total{operation="rotate"}') > 0 and not archives(server.access))
            finally:
                server.directory.chmod(0o700)
            server.get("/rotation-repaired")
            server.drain("/rotation-repaired")
            check("rotation resumes after directory permissions are repaired", len(archives(server.access)) == 1
                  and "/after-rotation-error" in content(archives(server.access)[0]))

        target, link = root/"symlink-target", root/"symlink.log"
        target.touch()
        link.symlink_to(target)
        with running(binary, root, policy="size=1k keep=2", access=link) as server:
            for n in range(10): server.get(f"/symlink-{n}")
            server.drain("/symlink-9")
            check("symlink log destinations are written without renaming or pruning the target", link.is_symlink()
                  and len(target.read_text().splitlines()) == 10 and not archives(link) and not archives(target))

        with running(binary, root, policy="size=1k", access=Path("/dev/stdout"), error=Path("stderr")) as server:
            output_path = server.directory/"process.log"
            output_path.chmod(0)
            try:
                result = subprocess.run([binary, "check", "-c", str(server.config)], stdout=server.output, stderr=server.output)
                server.get("/fail")
                wait_for(lambda: b'"GET /fail ' in os.pread(server.output.fileno(), 65536, 0))
                server.process.send_signal(signal.SIGUSR1)
                wait_for(lambda: server.metric("rgnix_log_reopens_total") >= 2)
                server.get("/inherited-stdout")
                wait_for(lambda: b"/inherited-stdout" in os.pread(server.output.fileno(), 65536, 0))
                check("inherited stdout/stderr work when filesystem permissions forbid reopening", result.returncode == 0
                      and b"request failed route=" in os.pread(server.output.fileno(), 65536, 0) and server.metric("rgnix_log_io_errors_total") == 0)
            finally:
                output_path.chmod(0o600)

        with running(binary, root) as server:
            server.get("/old-file")
            server.get("/fail")
            server.drain("/fail")
            old_access, old_error = server.directory/"old-access", server.directory/"old-error"
            server.access.rename(old_access)
            server.error.rename(old_error)
            server.process.send_signal(signal.SIGUSR1)
            wait_for(lambda: server.metric("rgnix_log_reopens_total") >= 2)
            server.get("/new-file")
            server.get("/fail")
            server.drain("/fail")
            check("USR1 reopens access and error files without restarting the server", "/new-file" in server.access.read_text()
                  and "/new-file" not in old_access.read_text() and server.error.read_text().count("request failed route=") == 1)
            before = server.metric("rgnix_log_reopens_total")
            server.access.rename(server.directory/"hup-old")
            server.reload("off")
            wait_for(lambda: server.metric("rgnix_log_reopens_total") > before)
            server.get("/hup-new")
            server.drain("/hup-new")
            check("valid SIGHUP also reopens local log files", "/hup-new" in server.access.read_text())
            old = server.directory/"still-writable"
            server.access.rename(old)
            server.access.mkdir()
            server.process.send_signal(signal.SIGUSR1)
            wait_for(lambda: server.metric('rgnix_log_io_errors_total{operation="reopen"}') > 0)
            server.get("/reopen-failed-kept")
            server.drain("/reopen-failed-kept", old)
            check("failed reopen keeps the old descriptor writable and reports the error", "/reopen-failed-kept" in old.read_text())
            server.access.rmdir()
            before = server.metric("rgnix_log_reopens_total")
            server.process.send_signal(signal.SIGUSR1)
            wait_for(lambda: server.metric("rgnix_log_reopens_total") >= before+2)
            server.get("/recovered")
            server.drain("/recovered")
            check("reopen recovers after the filesystem is repaired", "/recovered" in server.access.read_text())

        with running(binary, root) as server:
            assert shutil.which("logrotate"), "Install logrotate for the real external-rotation check"
            config = server.directory / "logrotate.conf"
            config.write_text(f'''{server.access} {{
                size 1
                rotate 2
                missingok
                notifempty
                compress
                delaycompress
                create 0640
                postrotate
                    kill -USR1 {server.process.pid}
                endscript
            }}\n''')
            for n in range(2):
                server.get(f"/external-{n}")
                server.drain(f"/external-{n}")
                before = server.metric("rgnix_log_reopens_total")
                subprocess.run([shutil.which("logrotate"), "--force", "--state", str(server.directory/"state"), str(config)], check=True, capture_output=True)
                wait_for(lambda: server.metric("rgnix_log_reopens_total") > before)
            server.get("/external-active")
            server.drain("/external-active")
            check("real logrotate rename/create/USR1 with delayed compression works", "/external-0" in gzip.decompress(Path(str(server.access)+".2.gz").read_bytes()).decode()
                  and "/external-1" in Path(str(server.access)+".1").read_text() and "/external-active" in server.access.read_text())

        # Force the single disk worker to block on a full FIFO, then reopen to a regular file.
        fifo = root/"backpressure.log"
        os.mkfifo(fifo)
        reader = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
        try:
            with running(binary, root, access=fifo) as server:
                for n in range(4300): assert server.get(f"/queue-{n}/" + "x"*512) == 200
                check("full local log queue drops records instead of blocking HTTP", server.metric("rgnix_access_logs_dropped_total") > 0 and server.metric("rgnix_file_logs_pending") <= 4097)
                fifo.rename(root/"old-pipe")
                fifo.touch()
                server.process.send_signal(signal.SIGUSR1)
                end = time.monotonic()+8
                while time.monotonic() < end and server.metric("rgnix_log_reopens_total") == 0:
                    try: os.read(reader, 65536)
                    except BlockingIOError: pass
                    time.sleep(.01)
                server.drain()
                server.get("/after-full-queue-reopen")
                server.drain("/after-full-queue-reopen")
                check("USR1 is not lost when the record queue is full", "/after-full-queue-reopen" in fifo.read_text())
        finally:
            os.close(reader)

    output = Path(os.environ.get("RGNIX_LOG_RESULTS", ".local/log-rotation.json"))
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps({"passed":len(RESULTS), "checks":RESULTS}, indent=2)+"\n")
    print(f"{len(RESULTS)} log rotation checks passed; {output}")


if __name__ == "__main__":
    main()
