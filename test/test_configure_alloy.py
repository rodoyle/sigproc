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

    def test_comments_and_other_scrapes_do_not_suppress_or_redirect_insert(self):
        other = 'prometheus.scrape "other" { targets = [\n' + alloy.ANCHOR + '\n' + alloy.TARGET + '\n] }\n'
        services = 'prometheus.scrape "services" {\n  targets = [\n    // ' + alloy.TARGET + '\n  ]\n}\n'
        prefix = '// prometheus.scrape "services" { targets = [] }\n' + other
        original = prefix + services
        updated = alloy.add_target(original)
        self.assertEqual(updated, prefix + services.replace('targets = [', 'targets = [\n' + alloy.TARGET))
        self.assertEqual(alloy.add_target(updated), updated)

    def test_existing_target_allows_harmless_spacing_and_field_order(self):
        original = 'prometheus.scrape "services" { targets=[ { job="sigproc-capture", "__address__"="sigproc-metrics.default.svc:9087" } ] }'
        self.assertEqual(alloy.add_target(original), original)
        conflict = original.replace('sigproc-metrics.default.svc:9087', 'wrong:9087')
        with self.assertRaises(ValueError):
            alloy.add_target(conflict)

    def test_unexpected_config_is_refused(self):
        for text in ('', alloy.ANCHOR, 'prometheus.scrape "services" {\n' + alloy.ANCHOR * 2):
            with self.subTest(text=text), self.assertRaises(ValueError):
                alloy.add_target(text)


if __name__ == '__main__':
    unittest.main()
