#!/usr/bin/env python3
"""Add the collector scrape to the existing Alloy ConfigMap without replacing other jobs.

Run after monitoring provisioning, then restart Alloy to load the mounted config.
Requires kubectl access to monitoring/configmap/alloy-config.
"""
import json
import subprocess
import sys

TARGET = '    { "__address__" = "sigproc-metrics.default.svc:9087", "job" = "sigproc-capture" },'
ANCHOR = '    { "__address__" = "127.0.0.1:12345", "job" = "alloy" },'


def add_target(config):
    if TARGET in config:
        return config
    if config.count(ANCHOR) != 1 or config.count('prometheus.scrape "services"') != 1:
        raise ValueError("unexpected Alloy services config; refusing to modify it")
    return config.replace(ANCHOR, ANCHOR + "\n" + TARGET, 1)


def kubectl(*args):
    return subprocess.run(["kubectl", "-n", "monitoring", *args], check=True,
                          capture_output=True, text=True, timeout=25).stdout


def main():
    try:
        cm = json.loads(kubectl("get", "configmap", "alloy-config", "-o", "json"))
        old = cm["data"]["config.alloy"]
        new = add_target(old)
        if new == old:
            print("Alloy target already configured; restart Alloy if config has not been loaded")
            return 0
        # Compare resourceVersion atomically; never overwrite a concurrent change.
        patch = [{"op": "test", "path": "/metadata/resourceVersion", "value": cm["metadata"]["resourceVersion"]},
                 {"op": "replace", "path": "/data/config.alloy", "value": new}]
        kubectl("patch", "configmap", "alloy-config", "--type=json", "-p", json.dumps(patch))
        print("Added collector scrape target; restart Alloy to load it")
        return 0
    except (subprocess.SubprocessError, KeyError, ValueError, json.JSONDecodeError) as exc:
        print(f"Alloy configuration not changed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
