"""Synthetic host credential tests. Never touches the user's credential store."""
import importlib.util
import json
import os
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('oauth_mirror', '/scripts/oauth_mirror.py')
mirror = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mirror)


class MirrorTests(unittest.TestCase):
    def test_access_only_atomic_refresh_logout_and_unchanged_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source, destination = root / 'host.json', root / 'mirror.json'
            value = {'claudeAiOauth': {'accessToken': 'synthetic-access', 'refreshToken': 'never-export-refresh',
                     'expiresAt': 9999999999999, 'scopes': ['user:inference']}, 'private_other_auth': 'never-copy'}
            source.write_text(json.dumps(value))
            source.chmod(0o600)
            before = source.read_bytes()
            mirror.sync(source, destination)
            self.assertEqual(source.read_bytes(), before)
            self.assertNotIn('never-export-refresh', destination.read_text())
            self.assertNotIn('never-copy', destination.read_text())
            self.assertEqual(destination.stat().st_mode & 0o777, 0o600)
            old_inode = destination.stat().st_ino
            value['claudeAiOauth']['accessToken'] = 'synthetic-rotated'
            replacement = root / 'replacement.json'
            replacement.write_text(json.dumps(value))
            replacement.chmod(0o600)
            os.replace(replacement, source)
            mirror.sync(source, destination)
            self.assertNotEqual(destination.stat().st_ino, old_inode)
            self.assertEqual(json.loads(destination.read_text())['claudeAiOauth']['accessToken'], 'synthetic-rotated')
            source.unlink()
            with self.assertRaises(mirror.CredentialError):
                mirror.sync(source, destination)
            self.assertFalse(destination.exists())

    def test_permissions_symlinks_and_malformed_data_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source, destination = root / 'host.json', root / 'mirror.json'
            source.write_text('invalid-json-secret')
            source.chmod(0o644)
            destination.write_text('stale')
            with self.assertRaises(mirror.CredentialError) as error:
                mirror.sync(source, destination)
            self.assertFalse(destination.exists())
            self.assertNotIn('invalid-json-secret', str(error.exception))
            source.chmod(0o600)
            with self.assertRaises(mirror.CredentialError):
                mirror.sync(source, destination)
            link = root / 'link.json'
            link.symlink_to(source)
            with self.assertRaises(mirror.CredentialError):
                mirror.sync(link, destination)


if __name__ == '__main__':
    unittest.main()
