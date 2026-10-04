#!/usr/bin/env python3
"""Exercise release deployment with isolated binaries and a fake user systemd."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT_ROOT = Path(__file__).resolve().parents[1]


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='agl-deployment-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / 'source tree'
        self.scripts = self.repo / 'scripts'
        self.scripts.mkdir(parents=True)
        for name in ('build-release.sh', 'install-agl-cargo.sh', 'restart-services.sh'):
            shutil.copy2(SCRIPT_ROOT / name, self.scripts / name)
        for name in ('systemd-lib.sh',
                     'agentlibre-daemon-systemd-service.sh',
                     'agentlibre-matrix-bridge-systemd-service.sh'):
            shutil.copy2(SCRIPT_ROOT / name, self.scripts / name)
        (self.repo / 'vendor/llama.cpp').mkdir(parents=True)
        (self.repo / 'vendor/llama.cpp/CMakeLists.txt').touch()
        (self.repo / 'assets').mkdir()
        (self.repo / 'assets/default-catalog.toml').write_text('format = 2\n')
        self.prefix = self.root / 'installed runtime'
        self.home = self.root / 'home'
        self.config = self.home / '.config'
        (self.config / 'agentLIBRE/matrix-bridge').mkdir(parents=True)
        (self.config / 'agentLIBRE/agentLIBRE.toml').write_text('')
        (self.config / 'agentLIBRE/matrix-bridge/agl.toml').write_text('')
        self.log = self.root / 'commands.jsonl'
        self.bin = self.root / 'mock bin'
        self.bin.mkdir()
        self.executable(self.bin / 'cargo', '''#!/usr/bin/env python3
import json, os, pathlib, sys
with open(os.environ['TEST_LOG'], 'a') as out:
    out.write(json.dumps(['cargo', os.getcwd(), *sys.argv[1:]]) + '\\n')
if os.environ.get('TEST_BUILD_FAIL') == '1': sys.exit(17)
target = pathlib.Path(os.environ['CARGO_TARGET_DIR'])
profile = 'release' if '--release' in sys.argv else 'debug'
(target / profile).mkdir(parents=True, exist_ok=True)
for name in ['agl', 'agl-matrix-bridge']:
    path = target / profile / name
    path.write_text('#!/bin/sh\\nif [ "$1" = forge ]; then\\n'
                    '  echo forge-sync >> "$TEST_LOG_FORGE"\\n'
                    '  [ "${TEST_SYNC_FAIL:-0}" != 1 ] || exit 19\\n'
                    '  printf "format = 1\\\\n" > agentLIBRE.lock\\n'
                    'fi\\necho new-' + name + '\\n')
    path.chmod(0o755)
''')
        self.executable(self.bin / 'systemctl', '''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ['TEST_LOG'], 'a') as out:
    out.write(json.dumps(['systemctl', *args]) + '\\n')
if 'show' in args and 'LoadState' in args:
    loaded = os.environ.get('TEST_UNITS', 'all')
    unit = args[args.index('show') + 1]
    print('loaded' if loaded == 'all' or unit in loaded.split(',') else 'not-found')
sys.exit(0)
''')
        self.executable(self.scripts / 'build-llama-cpp.sh', '''#!/usr/bin/env python3
import json, os, pathlib
with open(os.environ['TEST_LOG'], 'a') as out:
    out.write(json.dumps(['engine-build']) + '\\n')
path = pathlib.Path(os.environ['AGL_LLAMA_CPP_BUILD_DIR']) / 'bin'
path.mkdir(parents=True, exist_ok=True)
(path / 'llama-server').write_text('#!/bin/sh\\ntest -f "${0%/*}/libggml.so"\\n')
(path / 'llama-server').chmod(0o755)
if os.environ.get('TEST_NO_LIBS') != '1':
    (path / 'libggml.so').write_text('new engine library')
''')
        self.env = dict(os.environ, HOME=str(self.home),
                        XDG_CONFIG_HOME=str(self.config),
                        XDG_DATA_HOME=str(self.home / '.local/share'),
                        XDG_STATE_HOME=str(self.home / '.local/state'),
                        PATH=str(self.bin) + ':' + os.environ['PATH'],
                        TEST_LOG=str(self.log), AGL_NIX_VULKAN_ACTIVE='1',
                        TEST_LOG_FORGE=str(self.root / 'forge.log'),
                        CARGO_TARGET_DIR=str(self.root / 'build output'),
                        AGL_LLAMA_CPP_BUILD_DIR=str(self.root / 'engine build'))
        for name in ('CARGO_INSTALL_ROOT', 'AGL_MATRIX_BRIDGE_CONFIG', 'AGL_HOME'):
            self.env.pop(name, None)

    @staticmethod
    def executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def run_script(self, name='build-release.sh', args=(), **env):
        return subprocess.run([str(self.scripts / name), '--root', str(self.prefix), *args]
                              if name != 'restart-services.sh' else [str(self.scripts / name), *args],
                              cwd=self.root, env=dict(self.env, **env), text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def commands(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_complete_replacement_and_unit_refresh_from_another_directory(self):
        (self.prefix / 'bin').mkdir(parents=True)
        (self.prefix / 'libexec/agentlibre').mkdir(parents=True)
        self.executable(self.prefix / 'bin/agl', '#!/bin/sh\necho old\n')
        (self.prefix / 'libexec/agentlibre/libobsolete-backend.so').touch()
        generation = self.prefix / 'libexec/agentlibre/generations/immutable'
        generation.mkdir(parents=True)
        (generation / 'runtime-manifest.json').write_text('saved generation')
        generation.chmod(0o555)
        self.addCleanup(generation.chmod, 0o755)
        for _ in range(2):
            result = self.run_script()
            self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('new-agl', (self.prefix / 'bin/agl').read_text())
        self.assertFalse((self.prefix / 'libexec/agentlibre/libobsolete-backend.so').exists())
        self.assertEqual((generation / 'runtime-manifest.json').read_text(), 'saved generation')
        self.assertEqual(generation.stat().st_mode & 0o777, 0o555)
        commands = self.commands()
        cargo = next(c for c in commands if c[0] == 'cargo')
        self.assertEqual(cargo[1], str(self.repo))
        self.assertIn('--workspace', cargo)
        self.assertIn('--release', cargo)
        self.assertIn('--locked', cargo)
        self.assertEqual(sum(c == ['engine-build'] for c in commands), 2)
        stops = [i for i, c in enumerate(commands) if c[:3] == ['systemctl', '--user', 'stop']]
        self.assertGreater(stops[0], commands.index(['engine-build']))
        reload = max(i for i, c in enumerate(commands) if 'daemon-reload' in c)
        reset = max(i for i, c in enumerate(commands) if 'reset-failed' in c)
        start = max(i for i, c in enumerate(commands) if c[:3] == ['systemctl', '--user', 'start'])
        self.assertLess(reload, reset)
        self.assertLess(reset, start)
        units = self.config / 'systemd/user'
        for unit, binary in [('agentlibre-daemon.service', 'agl'),
                             ('agl-matrix-bridge.service', 'agl-matrix-bridge')]:
            self.assertIn(str(self.prefix / 'bin' / binary), (units / unit).read_text())
        self.assertIn('RestartPreventExitStatus=78', (units / 'agentlibre-daemon.service').read_text())

    def test_failed_build_or_missing_engine_libraries_never_stop_services(self):
        for env in ({'TEST_BUILD_FAIL': '1'}, {'TEST_NO_LIBS': '1'}):
            with self.subTest(env=env):
                if self.log.exists(): self.log.unlink()
                result = self.run_script(**env)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertFalse(any(c[:3] == ['systemctl', '--user', 'stop'] for c in self.commands()))
                self.assertFalse((self.prefix / 'bin/agl').exists())

    def test_fresh_install_without_matrix_and_stop_succeeds(self):
        (self.config / 'agentLIBRE/matrix-bridge/agl.toml').unlink()
        result = self.run_script(TEST_UNITS='')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.config / 'systemd/user/agl-matrix-bridge.service').exists())
        self.assertFalse(any('agl-matrix-bridge.service' in c and 'start' in c for c in self.commands()))
        result = self.run_script('restart-services.sh', ('--stop',), TEST_UNITS='agentlibre-daemon.service')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(['systemctl', '--user', 'stop', 'agentlibre-daemon.service'], self.commands())

    def test_binary_only_install_dry_run_and_no_force(self):
        result = self.run_script(args=('--dry-run',))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse(self.log.exists())
        self.assertFalse(self.prefix.exists())
        result = self.run_script(args=('--no-services',))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse(any(c[0] == 'systemctl' for c in self.commands()))
        self.assertFalse((self.config / 'systemd').exists())
        result = self.run_script('install-agl-cargo.sh', ('--no-force',))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('refusing to replace', result.stdout)

    def test_new_catalog_and_settings_use_agl_home_without_model_download(self):
        app = self.root / 'isolated agl home'
        result = self.run_script(args=('--no-services',), AGL_HOME=str(app))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual((app / 'data/source/agentLIBRE.toml').read_text(), 'format = 2\n')
        self.assertTrue((app / 'data/source/agentLIBRE.lock').is_file())
        settings = app / 'config/agentLIBRE.toml'
        self.assertIn('function:agentlibre.cpu-small@^1.0', settings.read_text())
        self.assertEqual(settings.stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.root / 'forge.log').read_text(), 'forge-sync\n')
        self.assertFalse((app / 'data/runtime/models').exists())

    def test_existing_catalog_and_settings_are_preserved_and_partial_pair_fails(self):
        catalog = self.home / '.local/share/agentLIBRE/source'
        catalog.mkdir(parents=True)
        manifest, lock = catalog / 'agentLIBRE.toml', catalog / 'agentLIBRE.lock'
        manifest.write_text('user declarations')
        settings = self.config / 'agentLIBRE/agentLIBRE.toml'
        settings.write_text('user settings')
        result = self.run_script(args=('--no-services',))
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn('incomplete catalog', result.stdout)
        self.assertFalse((self.prefix / 'bin/agl').exists())
        self.assertEqual(manifest.read_text(), 'user declarations')
        lock.write_text('user pins')
        result = self.run_script(args=('--no-services',))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(manifest.read_text(), 'user declarations')
        self.assertEqual(lock.read_text(), 'user pins')
        self.assertEqual(settings.read_text(), 'user settings')
        self.assertFalse((self.root / 'forge.log').exists())

    def test_initial_sync_failure_keeps_installed_runtime_and_requested_manifest(self):
        (self.prefix / 'bin').mkdir(parents=True)
        self.executable(self.prefix / 'bin/agl', '#!/bin/sh\necho old\n')
        result = self.run_script(TEST_SYNC_FAIL='1')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual((self.prefix / 'bin/agl').read_text(), '#!/bin/sh\necho old\n')
        catalog = self.home / '.local/share/agentLIBRE/source'
        self.assertTrue((catalog / 'agentLIBRE.toml').is_file())
        self.assertFalse((catalog / 'agentLIBRE.lock').exists())
        self.assertFalse(any(c[:3] == ['systemctl', '--user', 'stop'] for c in self.commands()))


if __name__ == '__main__':
    unittest.main()
