#!/usr/bin/env python3
"""Failure handoff and readiness lifecycle tests for the overnight runner."""
import argparse
import importlib.util
import io
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('full_prefill', Path(__file__).with_name('run-full-prefill.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RunnerTests(unittest.TestCase):
    def test_conversion_failure_stops_pipeline_and_records_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            converter = root / 'converter.py'
            converter.write_text('import sys\nprint("fixture conversion failed", flush=True)\nsys.exit(23)\n')
            out = root / 'run'
            result = subprocess.run([
                sys.executable, str(Path(module.__file__)), '--bin', '/does-not-exist',
                '--converter', str(converter), '--source', str(root), '--base', '/missing',
                '--key', '/missing', '--out', str(out), '--sponsor', 'fixture'],
                capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 1)
            status = json.loads((out / 'status.json').read_text())
            self.assertEqual(status['state'], 'failed')
            self.assertEqual(len(status['phases']), 1)
            self.assertEqual(status['phases'][0]['exitCode'], 23)
            self.assertIn('fixture conversion failed', (out / 'convert.log').read_text())
            self.assertFalse((out / 'stream-build.log').exists())

    def test_expiry_policy_is_forwarded_to_converter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            converter = root / 'converter.py'
            converter.write_text('import json, sys\nprint(json.dumps(sys.argv[1:]), flush=True)\nsys.exit(7)\n')
            out = root / 'run'
            result = subprocess.run([
                sys.executable, str(Path(module.__file__)), '--bin', '/does-not-exist',
                '--converter', str(converter), '--source', str(root), '--base', '/missing',
                '--key', '/missing', '--out', str(out), '--sponsor', 'fixture',
                '--expiry-policy', 'far-future', '--max-expiry', '1000000000000'],
                capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads((out / 'status.json').read_text())['phases'][0]['exitCode'], 7)
            argv = json.loads((out / 'convert.log').read_text())
            self.assertEqual(argv[argv.index('--expiry-policy') + 1], 'far-future')
            self.assertEqual(argv[argv.index('--max-expiry') + 1], '1000000000000')
            default = root / 'default'
            subprocess.run([
                sys.executable, str(Path(module.__file__)), '--bin', '/does-not-exist',
                '--converter', str(converter), '--source', str(root), '--base', '/missing',
                '--key', '/missing', '--out', str(default), '--sponsor', 'fixture'],
                capture_output=True, text=True, timeout=15)
            argv = json.loads((default / 'convert.log').read_text())
            self.assertEqual(argv, ['convert', '--bundle', str(root), '--out', str(default / 'converted'),
                                    '--reserve-gib', '80'])

    def test_unlimited_policy_is_forwarded_without_finite_options(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            converter = root / 'converter.py'
            converter.write_text('import json, sys\nprint(json.dumps(sys.argv[1:]), flush=True)\nsys.exit(7)\n')
            common = [sys.executable, str(Path(module.__file__)), '--bin', '/does-not-exist',
                      '--converter', str(converter), '--source', str(root), '--base', '/missing',
                      '--key', '/missing', '--sponsor', 'fixture', '--expiry-policy', 'unlimited']
            out = root / 'run'
            result = subprocess.run([*common, '--out', str(out)], capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 1)
            argv = json.loads((out / 'convert.log').read_text())
            self.assertEqual(argv[argv.index('--expiry-policy') + 1], 'unlimited')
            self.assertNotIn('--max-expiry', argv)
            rejected = root / 'rejected'
            result = subprocess.run([*common, '--out', str(rejected), '--max-expiry', '100'],
                                    capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 2)
            self.assertIn('does not apply', result.stderr)
            self.assertFalse(rejected.exists())

    def test_unlimited_bundle_must_match_conversion_accounting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            converter = root / 'converter.py'
            # Publishes a conversion report, then the fake builder publishes a
            # bundle report whose burn disagrees with it.
            converter.write_text(
                'import json, pathlib, sys\n'
                'out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])\n'
                'out.mkdir()\n'
                'report = {"streamHash": "0x01", "rentWei": "0", "storageUnits": "5"}\n'
                '(out / "conversion-report.json").write_text(json.dumps(report))\n')
            builder = root / 'builder.py'
            builder.write_text(
                '#!' + sys.executable + '\n'
                'import json, pathlib, sys\n'
                'out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])\n'
                'out.mkdir()\n'
                'report = {"streamHash": "0x01", "burnedWei": "1", "storageUnits": "5", "permanentPrefill": True}\n'
                '(out / "report.json").write_text(json.dumps(report))\n')
            builder.chmod(0o755)
            out = root / 'run'
            with patch.object(module.time, 'sleep', lambda _seconds: None):
                runner = module.Runner(argparse.Namespace(
                    out=out, converter=converter, source=root, reserve_gib=0, expiry_policy='unlimited',
                    max_expiry=None, bin=builder, base=root / 'base', sponsor='fixture', key=root / 'key',
                    rebuild_genesis_in_producer=False))
                with self.assertRaisesRegex(RuntimeError, 'does not match the conversion report'):
                    runner.run()
            self.assertEqual([phase['name'] for phase in runner.status['phases']], ['convert', 'stream-build'])

    def test_verified_producer_keeps_running_until_signal(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = module.Runner(argparse.Namespace(out=Path(directory) / 'run'))
            observed = []
            def interrupt(_seconds):
                observed.append(runner.status['state'])
                self.assertIsNone(runner.child.poll())
                runner.stop(signal.SIGTERM, None)
            try:
                with patch.object(module.time, 'sleep', interrupt):
                    with self.assertRaises(InterruptedError):
                        runner.phase('producer', [sys.executable, '-c', 'import time; time.sleep(60)'],
                                     ready=lambda: {'head': {'height': '0'}})
            finally:
                runner.child.wait(timeout=10)
            self.assertEqual(observed, ['ready'])
            self.assertEqual(runner.child.returncode, -signal.SIGTERM)
            self.assertIn('readySeconds', runner.status['phases'][0])

    def run_with_fake_bin(self, root, *extra):
        """Convert and stream-build succeed; the producer exits so argv can be checked."""
        converter = root / 'converter.py'
        converter.write_text('import sys\nsys.exit(0)\n')
        calls = root / 'calls.jsonl'
        fake = root / 'arkiv-proposer'
        fake.write_text(f"""#!{sys.executable}
import json, sys
from pathlib import Path
argv = sys.argv[1:]
with open({str(calls)!r}, 'a') as log:
    log.write(json.dumps(argv) + '\\n')
if argv[:2] == ['genesis', 'stream-build']:
    out = Path(argv[argv.index('--out') + 1])
    out.mkdir()
    (out / 'report.json').write_text(json.dumps({{'genesisHash': '0x01', 'stateRoot': '0x02'}}))
    if '--state-out' in argv:
        Path(argv[argv.index('--state-out') + 1]).mkdir()
    sys.exit(0)
sys.exit(9)
""")
        fake.chmod(0o755)
        out = root / 'run'
        result = subprocess.run([
            sys.executable, str(Path(module.__file__)), '--bin', str(fake),
            '--converter', str(converter), '--source', str(root), '--base', str(root / 'base.bin'),
            '--key', str(root / 'key'), '--out', str(out), '--sponsor', '0x01', *extra],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 1, result.stderr)
        status = json.loads((out / 'status.json').read_text())
        self.assertEqual([p['name'] for p in status['phases']], ['convert', 'stream-build', 'producer'])
        self.assertEqual(status['phases'][2]['exitCode'], 9)
        build, producer = [json.loads(line) for line in calls.read_text().splitlines()]
        return out, status, build, producer

    def test_build_once_hands_built_state_to_producer(self):
        with tempfile.TemporaryDirectory() as directory:
            out, status, build, producer = self.run_with_fake_bin(Path(directory))
            state = str(out / 'genesis-state')
            self.assertEqual(status['genesisBuild'], 'once')
            self.assertEqual(build[build.index('--state-out') + 1], state)
            self.assertEqual(producer[0], 'producer')
            self.assertEqual(producer[producer.index('--prebuilt-state') + 1], state)
            self.assertEqual(producer[producer.index('--bundle') + 1], str(out / 'bundle'))

    def test_producer_rebuild_remains_available(self):
        with tempfile.TemporaryDirectory() as directory:
            _, status, build, producer = self.run_with_fake_bin(
                Path(directory), '--rebuild-genesis-in-producer')
            self.assertEqual(status['genesisBuild'], 'producer-rebuild')
            self.assertNotIn('--state-out', build)
            self.assertNotIn('--prebuilt-state', producer)

    def test_readiness_requires_bundle_head_and_prebuilt_install_without_rebuild(self):
        expected = {'genesisHash': '0xaa', 'stateRoot': '0xbb'}
        good = {'head': {'height': '0', 'hash': '0xaa', 'stateRoot': '0xbb'}}
        self.assertIs(module.verify_ready(good, expected), good)
        for head in ({'height': '1', 'hash': '0xaa', 'stateRoot': '0xbb'},
                     {'height': '0', 'hash': '0xab', 'stateRoot': '0xbb'},
                     {'height': '0', 'hash': '0xaa', 'stateRoot': '0xbc'}):
            with self.assertRaisesRegex(RuntimeError, 'does not match'):
                module.verify_ready({'head': head}, expected)
        install = '{"event":"prefill_prebuilt_install","result":"%s"}'
        audit = '{"event":"prefill_progress","stage":"audit","rows":0,"complete":false}'
        rebuild = '{"event":"prefill_progress","stage":"bulk_normalize","rows":0,"complete":false}'
        phase = '{"count":0,"event":"prefill_bulk_phase","phase":"keys"}'
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / 'producer.log'
            for lines, accepted in (([audit, install % 'moved', audit, 'not json', 'arkiv-proposer: started'], True),
                                    ([install % 'copied'], True),
                                    ([audit], False),
                                    ([install % 'existing'], False),
                                    ([install % 'moved', install % 'moved'], False),
                                    ([install % 'moved', rebuild], False),
                                    ([install % 'moved', phase], False)):
                log.write_text('\n'.join(lines) + '\n')
                if accepted:
                    self.assertIs(module.verify_ready(good, expected, log), good)
                else:
                    with self.assertRaisesRegex(RuntimeError, 'prebuilt genesis state'):
                        module.verify_ready(good, expected, log)

    def test_existing_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileExistsError):
                module.Runner(argparse.Namespace(out=Path(directory)))

    def test_genesis_install_is_forwarded_to_the_producer_only_for_adopt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            converter = root / 'converter.py'
            converter.write_text('import pathlib, sys\n'
                                 'pathlib.Path(sys.argv[sys.argv.index("--out") + 1]).mkdir()\n')
            # Builds a bundle report, then records the producer command line and fails.
            binary = root / 'bin.py'
            binary.write_text(
                '#!' + sys.executable + '\n'
                'import json, pathlib, sys\n'
                'args = sys.argv[1:]\n'
                'if args[:2] == ["genesis", "stream-build"]:\n'
                '    out = pathlib.Path(args[args.index("--out") + 1])\n'
                '    out.mkdir()\n'
                '    (out / "report.json").write_text(json.dumps({"genesisHash": "0x1", "stateRoot": "0x2"}))\n'
                'else:\n'
                '    print(json.dumps(args), flush=True)\n'
                '    sys.exit(3)\n')
            binary.chmod(0o755)
            for install in (None, 'copy', 'adopt', 'move'):
                out = root / f'run-{install}'
                argv = ['run-full-prefill.py', '--bin', str(binary), '--converter', str(converter),
                        '--source', str(root), '--base', str(root / 'base'), '--key', str(root / 'key'),
                        '--out', str(out), '--sponsor', 'fixture', '--reserve-gib', '0']
                if install is not None:
                    argv += ['--genesis-install', install]
                # The runner's own parser supplies every option's default.
                with patch.object(module.sys, 'argv', argv), \
                        patch.object(module.time, 'sleep', lambda _seconds: None), \
                        patch.object(module.signal, 'signal', lambda *_args: None), \
                        patch.dict(module.os.environ):
                    if install == 'move':
                        with self.assertRaises(SystemExit) as refused, \
                                patch.object(module.sys, 'stderr', io.StringIO()):
                            module.main()
                        self.assertEqual(refused.exception.code, 2)
                        self.assertFalse(out.exists())
                        continue
                    with patch.object(module.sys, 'stderr', io.StringIO()):
                        self.assertEqual(module.main(), 1)
                status = json.loads((out / 'status.json').read_text())
                self.assertIn('producer exited with code 3', status['error'])
                argv = json.loads((out / 'producer.log').read_text())
                self.assertEqual(argv[0], 'producer')
                if install == 'adopt':
                    self.assertEqual(argv[argv.index('--genesis-install') + 1], 'adopt')
                    self.assertEqual(status['genesisInstall'], 'adopt')
                else:
                    self.assertNotIn('--genesis-install', argv)
                    self.assertNotIn('genesisInstall', status)

if __name__ == '__main__':
    unittest.main()
