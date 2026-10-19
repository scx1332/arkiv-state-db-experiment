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
