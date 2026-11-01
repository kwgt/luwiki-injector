#!/usr/bin/python3
# Page injector for kwgt/luwiki
# Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
"""合成ZIP/loopback専用のPTY実CLI制御。実資格情報を受け取らない。"""

import argparse
import errno
import fcntl
import os
import pathlib
import pty
import selectors
import signal
import stat
import subprocess
import sys
import termios
import time
import urllib.parse


LIMIT = 1_048_576
PROMPT = b"Wiki Basic password: "
PASSWORD = b"synthetic-password-only"


class ProtocolFailure(Exception):
    """入力値を表示しない固定protocol失敗。"""


class Parser(argparse.ArgumentParser):
    """argparseの入力値/usageを生表示しないparser。"""

    def error(self, _message):
        """値を含む元診断を保持せず固定失敗を返す。"""
        raise ProtocolFailure()


def arguments():
    """固定オプションを読み、合成loopback/ZIP条件を検査する。"""
    parser = Parser(add_help=False)
    for name in ("binary", "config", "source", "url", "prefix", "status"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--password-mode", choices=("valid", "empty"),
                        required=True)
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.url)
    if (url.scheme != "http" or url.hostname != "127.0.0.1"
            or not url.port or url.username is not None
            or url.password is not None or url.path not in ("", "/")
            or url.query or url.fragment or args.prefix != "/fixture"):
        raise ProtocolFailure()
    for name in ("binary", "config", "source", "status"):
        path = pathlib.Path(getattr(args, name))
        if not path.is_absolute() or not stat.S_ISREG(path.lstat().st_mode):
            raise ProtocolFailure()
    if pathlib.Path(args.source).suffix != ".zip":
        raise ProtocolFailure()
    return args


def status_record():
    """元値を含まない初期状態を返す。未知exitを0へ補完しない。"""
    return dict(schema=1, wrapper="protocol", prompt_seen=False,
                echo_seen=False, timed_out=False, capture_rejected=False,
                reaped=False)


def write_status(path, record):
    """自分の0700領域の既存0600 fileへ4KiB以下のTOMLを書く。"""
    parent = pathlib.Path(path).parent.lstat()
    if (not stat.S_ISDIR(parent.st_mode) or parent.st_mode & 0o077
            or parent.st_uid != os.geteuid()):
        raise ProtocolFailure()
    fd = os.open(path, os.O_WRONLY | os.O_NOFOLLOW)
    try:
        meta = os.fstat(fd)
        if (not stat.S_ISREG(meta.st_mode) or meta.st_mode & 0o077
                or meta.st_uid != os.geteuid() or meta.st_size > 4096):
            raise ProtocolFailure()
        lines = []
        for key, value in record.items():
            if isinstance(value, bool):
                rendered = "true" if value else "false"
            elif isinstance(value, int):
                rendered = str(value)
            else:
                rendered = '"' + value + '"'
            lines.append(key + " = " + rendered + "\n")
        data = "".join(lines).encode("ascii")
        if len(data) > 4096:
            raise ProtocolFailure()
        os.ftruncate(fd, 0)
        offset = 0
        while offset < len(data):
            offset += os.write(fd, data[offset:])
        os.fsync(fd)
    finally:
        os.close(fd)


def isolated_terminal():
    """製品子だけのsession/control TTYをexec前に作る。"""
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def stop_group(child, selected_signal):
    """まだreapしていない、このhelper所有sessionだけを停止する。"""
    try:
        os.killpg(child.pid, selected_signal)
    except ProcessLookupError:
        pass


def exited_without_reaping(child):
    """PID再利用を避けるためWNOWAITで終了だけを観測する。"""
    return os.waitid(os.P_PID, child.pid,
                     os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def run(args, record):
    """3streamを有限drainし、孤立したCLI子を必ず回収する。"""
    master, slave = pty.openpty()
    child = None
    selector = selectors.DefaultSelector()
    buffers = {"stdout": bytearray(), "stderr": bytearray(),
               "tty": bytearray()}
    password = PASSWORD if args.password_mode == "valid" else b""
    termination_at = None
    sent = False
    try:
        # prompt前のraceを含め、入力を送る前からslave ECHOを無効にする。
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~(termios.ECHO | termios.ECHONL)
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
        environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                       "RUST_LOG": "trace", "LUWIKI_USER": "synthetic-user",
                       "XDG_CONFIG_HOME": str(pathlib.Path(args.config).parent)}
        command = [args.binary, "--config-path", args.config,
                   "--prefix", "/fixture", "--mode", "update",
                   args.source, args.url]
        child = subprocess.Popen(command, stdin=slave, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, env=environment,
                                 shell=False, close_fds=True,
                                 preexec_fn=isolated_terminal)
        os.close(slave)
        slave = None
        for fd, name in ((child.stdout.fileno(), "stdout"),
                         (child.stderr.fileno(), "stderr"), (master, "tty")):
            os.set_blocking(fd, False)
            selector.register(fd, selectors.EVENT_READ, name)
        deadline = time.monotonic() + 30.0
        # stdout/stderr/control TTYを分け、超過後も保持せず読み捨てる。
        while selector.get_map() or not exited_without_reaping(child):
            now = time.monotonic()
            if now >= deadline:
                record["timed_out"] = True
            failed = (record["timed_out"] or record["capture_rejected"]
                      or record["echo_seen"])
            if failed and termination_at is None:
                stop_group(child, signal.SIGTERM)
                termination_at = now
            if termination_at is not None and now - termination_at >= 2.0:
                stop_group(child, signal.SIGKILL)
                # KILL後もpipeを保持する子孫のEOFを無期限に待たない。
                if now - termination_at >= 3.0:
                    record["capture_rejected"] = True
                    break
            for key, _mask in selector.select(0.05):
                try:
                    chunk = os.read(key.fd, 8192)
                except OSError as error:
                    if key.data == "tty" and error.errno == errno.EIO:
                        chunk = b""
                    elif error.errno in (errno.EAGAIN, errno.EINTR):
                        continue
                    else:
                        raise
                if not chunk:
                    selector.unregister(key.fd)
                    continue
                buf = buffers[key.data]
                if len(buf) + len(chunk) > LIMIT:
                    record["capture_rejected"] = True
                    buf.clear()
                elif not record["capture_rejected"]:
                    buf.extend(chunk)
                if password and password in buf:
                    record["echo_seen"] = True
                if not sent and PROMPT in buf:
                    record["prompt_seen"] = True
                    sent = True
                    os.write(master, password + b"\n")
            if exited_without_reaping(child):
                # pipe EOFも回収するまでPIDをreapせず保持する。
                continue
        code = child.wait()
        record["reaped"] = True
        if code < 0:
            record["child_signal"] = -code
        else:
            record["child_exit"] = code
        if record["timed_out"]:
            record["wrapper"] = "timeout"
        elif record["echo_seen"]:
            record["wrapper"] = "echo"
        elif record["capture_rejected"]:
            record["wrapper"] = "capture"
        else:
            record["wrapper"] = "ok"
    except OSError:
        record["wrapper"] = "spawn" if child is None else "io"
    except Exception:
        record["wrapper"] = "protocol"
    finally:
        # 失敗時も子と3streamを残したまま成功結果を返さない。
        if child is not None and not record["reaped"]:
            stop_group(child, signal.SIGKILL)
            code = child.wait()
            record["reaped"] = True
            record["child_signal" if code < 0 else "child_exit"] = abs(code)
        selector.close()
        if child is not None:
            child.stdout.close()
            child.stderr.close()
        if slave is not None:
            os.close(slave)
        os.close(master)
    return buffers


def main():
    """固定protocolだけを返し、raw error/Command/typescriptを保存しない。"""
    record = status_record()
    args = None
    buffers = None
    try:
        args = arguments()
        buffers = run(args, record)
        write_status(args.status, record)
        if record["wrapper"] != "ok":
            raise ProtocolFailure()
        # 送信前に両streamのUTF8を確認し、片側だけを転送しない。
        buffers["stdout"].decode("utf-8", "strict")
        buffers["stderr"].decode("utf-8", "strict")
        sys.stdout.buffer.write(buffers["stdout"])
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(buffers["stderr"])
        sys.stderr.buffer.flush()
        return 0
    except Exception:
        if args is not None:
            if record["wrapper"] == "ok":
                record["wrapper"] = "protocol"
            try:
                write_status(args.status, record)
            except Exception:
                pass
        sys.stderr.write("MS51 PTY wrapper_failed=true recovery=unknown\n")
        return 1


if __name__ == "__main__":
    sys.exit(main())
