#!/usr/bin/env python3
"""Add the collector scrape to the existing Alloy ConfigMap without replacing other jobs.

Run after monitoring provisioning, then restart Alloy to load the mounted config.
Requires kubectl access to monitoring/configmap/alloy-config.
"""
import json
import re
import subprocess
import sys

TARGET = '    { "__address__" = "sigproc-metrics.default.svc:9087", "job" = "sigproc-capture" },'
ANCHOR = '    { "__address__" = "127.0.0.1:12345", "job" = "alloy" },'


def tokens(config):
    # Keep source offsets while ignoring comments and treating strings atomically.
    pattern = r'//[^\n]*|\#[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|[A-Za-z_][A-Za-z0-9_.]*|[^\s]'
    return [(m.group(), m.start(), m.end()) for m in re.finditer(pattern, config, re.S)
            if not m.group().startswith(('//', '#', '/*'))]


def closing(items, start, opener, closer):
    depth = 0
    for i in range(start, len(items)):
        if items[i][0] == opener:
            depth += 1
        elif items[i][0] == closer:
            depth -= 1
            if depth == 0:
                return i
    raise ValueError("unclosed Alloy block/list; refusing to modify it")


def add_target(config):
    items = tokens(config)
    starts = [i + 2 for i in range(len(items) - 2)
              if [t[0] for t in items[i:i + 3]] == ['prometheus.scrape', '"services"', '{']]
    if len(starts) != 1:
        raise ValueError("expected exactly one Alloy services scrape")
    start = starts[0]
    end = closing(items, start, '{', '}')
    lists = []
    depth = 0
    for i in range(start, end):
        value = items[i][0]
        if value == '{':
            depth += 1
        elif value == '}':
            depth -= 1
        elif depth == 1 and [t[0] for t in items[i:i + 3]] == ['targets', '=', '[']:
            lists.append(i + 2)
    if len(lists) != 1:
        raise ValueError("services scrape must have one literal targets list")
    list_start = lists[0]
    list_end = closing(items, list_start, '[', ']')
    if list_end >= end:
        raise ValueError("targets list extends outside services scrape")
    i = list_start + 1
    while i < list_end:
        if items[i][0] == ',':
            i += 1
            continue
        if items[i][0] != '{':
            raise ValueError("targets must be literal maps")
        entry_end = closing(items, i, '{', '}')
        if entry_end >= list_end:
            raise ValueError("invalid target map")
        fields = {}
        for j in range(i + 1, entry_end - 2):
            if items[j + 1][0] == '=' and items[j + 2][0].startswith('"'):
                try:
                    fields[items[j][0].strip('"')] = json.loads(items[j + 2][0])
                except json.JSONDecodeError as exc:
                    raise ValueError("invalid target string") from exc
        address = fields.get('__address__')
        job = fields.get('job')
        if address == 'sigproc-metrics.default.svc:9087' and job == 'sigproc-capture':
            return config
        if address == 'sigproc-metrics.default.svc:9087' or job == 'sigproc-capture':
            raise ValueError("conflicting collector target in services scrape")
        i = entry_end + 1
    # Insert only inside the intended list; comments/other scrapes are untouched.
    position = items[list_start][2]
    return config[:position] + '\n' + TARGET + config[position:]


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
