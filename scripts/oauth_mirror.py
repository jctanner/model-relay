#!/usr/bin/env python3
"""Read host credentials; atomically project access-only credentials for Podman.

Never writes the source, exports a refresh token, calls a provider, or prints
credential values. A directory mount follows atomic replacement of the mirror.
"""
import argparse
import fcntl
import json
import os
import pathlib
import signal
import stat
import tempfile
import threading


class CredentialError(Exception):
    pass


def project(source):
    try:
        descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise CredentialError('Host credentials must be an owner-only regular file')
            if not 0 < info.st_size <= 1024 * 1024:
                raise CredentialError('Invalid host credential size')
            raw = stream.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise CredentialError('Invalid host credential size')
            value = json.loads(raw)
        native = value.get('claudeAiOauth') if isinstance(value, dict) else None
        if not isinstance(native, dict):
            raise CredentialError('Native Claude OAuth login is unavailable')
        token, expires, scopes = (native.get(key) for key in ('accessToken', 'expiresAt', 'scopes'))
        if (not isinstance(token, str) or not 0 < len(token) <= 16384
                or not all(33 <= ord(char) <= 126 for char in token)
                or type(expires) is not int or not 0 < expires <= 2**63 - 1
                or not isinstance(scopes, list) or not 0 < len(scopes) <= 100
                or not all(isinstance(s, str) and 0 < len(s) <= 200
                           and all(33 <= ord(c) <= 126 for c in s) for s in scopes)
                or 'user:inference' not in scopes):
            raise CredentialError('Native Claude OAuth credentials are incomplete')
        return {'claudeAiOauth': {'accessToken': token, 'expiresAt': expires, 'scopes': scopes}}
    except (OSError, ValueError, UnicodeError):
        raise CredentialError('Cannot read valid host Claude OAuth credentials') from None


def write_projection(destination, value):
    payload = json.dumps(value, separators=(',', ':')).encode() + b'\n'
    try:
        if destination.read_bytes() == payload and destination.stat().st_mode & 0o777 == 0o600:
            return
    except FileNotFoundError:
        pass
    descriptor, name = tempfile.mkstemp(prefix='.mirror-', dir=destination.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, destination)
        directory = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def sync(source, destination):
    try:
        value = project(source)
    except CredentialError:
        destination.unlink(missing_ok=True)
        raise
    write_projection(destination, value)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True, type=pathlib.Path)
    parser.add_argument('--directory', required=True, type=pathlib.Path)
    parser.add_argument('--once', action='store_true')
    args = parser.parse_args()
    args.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = args.directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise SystemExit('OAuth mirror directory must be owned by this user with mode 0700')
    destination = args.directory / '.credentials.json'
    # Never overwrite a source credential file or use the host Claude directory.
    if args.source.parent.resolve() == args.directory.resolve():
        raise SystemExit('OAuth mirror must be separate from the source directory')
    lock_path = args.directory / '.vertex-shim-owner.lock'
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'rb') as owner:
        try:
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit('Another process owns this OAuth mirror') from None
        stopped = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_: stopped.set())
        signal.signal(signal.SIGINT, lambda *_: stopped.set())
        previous = None
        while not stopped.is_set():
            try:
                sync(args.source, destination)
                state = 'Host access credential mirrored; refresh remains with the host CLI.'
            except CredentialError as error:
                state = str(error)
                if args.once:
                    raise SystemExit(state) from None
            if state != previous:
                print(state, flush=True)
                previous = state
            if args.once:
                return
            stopped.wait(0.5)
        # Prevent a stopped/stale watcher from silently leaving a usable token.
        destination.unlink(missing_ok=True)


if __name__ == '__main__':
    main()
