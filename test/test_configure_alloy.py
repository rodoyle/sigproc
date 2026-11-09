import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("configure_alloy", Path(__file__).resolve().parents[1] / "deploy" / "configure-alloy.py")
assert spec is not None and spec.loader is not None
alloy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(alloy)


class AlloyConfigTests(unittest.TestCase):
    def test_add_is_idempotent_and_preserves_other_config(self):
        original = 'prometheus.scrape "services" {\n  targets = [\n' + alloy.ANCHOR + '\n  ]\n}\nother configuration\n'
        updated = alloy.add_target(original)
        self.assertEqual(updated.replace('\n' + alloy.TARGET, ''), original)
        self.assertEqual(alloy.add_target(updated), updated)

    def test_unexpected_config_is_refused(self):
        for text in ('', alloy.ANCHOR, 'prometheus.scrape "services" {\n' + alloy.ANCHOR * 2):
            with self.subTest(text=text), self.assertRaises(ValueError):
                alloy.add_target(text)


if __name__ == '__main__':
    unittest.main()
