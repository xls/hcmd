"""Drive the release binary over a pty: start, list, view, quit.

The failure this exists to catch is a refactor that passes every test and
cannot start, which no unit test can see because none of them own a terminal.
"""
import os, pty, select, shutil, struct, sys, termios, fcntl, time

# Absolute: the child chdirs into the fixture before exec.
BIN = os.path.abspath("target/release/hcmd")
ROOT = "/tmp/hcmd-smoke"
COLS, ROWS = 100, 24


def main() -> int:
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)
    with open(os.path.join(ROOT, "smoke.txt"), "w") as f:
        f.write("smoke test payload\n" * 5)
    for d in ("/tmp/hcmd-smoke-cfg", "/tmp/hcmd-smoke-state"):
        shutil.rmtree(d, ignore_errors=True)
    env = dict(os.environ)
    env.update({
        "TERM": "xterm-256color", "HOME": ROOT,
        "XDG_CONFIG_HOME": "/tmp/hcmd-smoke-cfg",
        "XDG_STATE_HOME": "/tmp/hcmd-smoke-state",
        "HCMD_KEYBOARD_PROTOCOL": "enhanced",
    })
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(ROOT)
        os.dup2(os.open(os.devnull, os.O_WRONLY), 2)
        os.execve(BIN, [BIN], env)
        os._exit(1)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    buf = bytearray()

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(fd, 1 << 16)
                except OSError:
                    return
                if not chunk:
                    return
                buf.extend(chunk)

    failures = []
    pump(2.5)
    text = buf.decode("utf-8", "replace")
    if "smoke" not in text:
        failures.append("the panel did not list the fixture directory")

    buf.clear()
    os.write(fd, b"smoke")
    pump(0.5)
    os.write(fd, b"\x1b[13~")          # F3
    pump(2.0)
    if "smoke test payload" not in buf.decode("utf-8", "replace"):
        failures.append("the viewer did not show the file")

    buf.clear()
    os.write(fd, b"q")                  # close the viewer
    pump(1.0)
    os.write(fd, b"\x1b[21~")           # F10 quits
    pump(0.8)
    # `ui.confirm_exit` may put a confirmation in the way; Enter accepts it and
    # is harmless when there is none, because the panel has already gone.
    os.write(fd, b"\r")
    pump(1.2)

    deadline = time.time() + 5
    status = None
    while time.time() < deadline:
        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            status = st
            break
        time.sleep(0.1)
    if status is None:
        os.kill(pid, 9)
        failures.append("it did not exit on F10")
    elif os.WIFSIGNALED(status):
        failures.append(f"it died on signal {os.WTERMSIG(status)}")
    elif os.WEXITSTATUS(status) != 0:
        failures.append(f"it exited {os.WEXITSTATUS(status)}")

    if failures:
        for f in failures:
            print("FAIL:", f)
        return 1
    print("SMOKE-OK started, listed, viewed and exited cleanly")
    return 0


if __name__ == "__main__":
    sys.exit(main())
