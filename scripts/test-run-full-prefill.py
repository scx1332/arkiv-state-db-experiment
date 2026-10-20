#!/usr/bin/env python3
"""Failure handoff and readiness lifecycle tests for the overnight runner."""
import argparse
import importlib.util
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
                    max_expiry=None, bin=builder, base=root / 'base', sponsor='fixture', key=root / 'key'))
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

    def test_existing_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileExistsError):
                module.Runner(argparse.Namespace(out=Path(directory)))


if __name__ == '__main__':
    unittest.main()
