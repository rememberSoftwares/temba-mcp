#!/usr/bin/env python3
"""Exercise Helm render-time catalog validation without accessing a cluster."""
import json
from pathlib import Path
import subprocess
import tempfile

chart = Path(__file__).resolve().parents[1] / 'chart' / 'temba-catalog'

def server(identifier, endpoint='https://mcp.example.com/mcp', **extra):
    return {'id': identifier, 'endpoint': endpoint, **extra}

cases = [
    ('empty catalog', [], None),
    ('unique IDs', [server('service-a'), server('service-b')], None),
    ('case-sensitive IDs', [server('Service'), server('service')], None),
    ('same ID, different endpoint', [server('same'), server('same', 'https://other.example.com/mcp')], 'Duplicate MCP id "same"'),
    ('identical entries', [server('same'), server('same')], 'Duplicate MCP id "same"'),
    ('missing ID', [{'endpoint': 'https://mcp.example.com/mcp'}], '.id must be a nonempty string'),
    ('numeric ID', [server(123)], '.id must be a nonempty string'),
    ('empty ID', [server('')], '.id is invalid'),
    ('ID with spaces', [server('Service A')], '.id is invalid'),
    ('reserved local ID', [server('__local_reserved')], '.id is invalid'),
    ('ID too long', [server('a' * 65)], '.id is invalid'),
    ('legacy display name', [server('service', name='Service')], '.name is no longer supported'),
    ('header name remains supported', [server('service', headers=[{'name': 'X-Tenant', 'value': 'team'}])], None),
]
with tempfile.TemporaryDirectory(prefix='temba-chart-check-') as directory:
    values = Path(directory) / 'values.json'
    for title, entries, error in cases:
        values.write_text(json.dumps({'catalog': {'servers': entries}}))
        result = subprocess.run(['helm', 'template', 'check', str(chart), '-f', str(values)], text=True, capture_output=True)
        if error is None:
            assert result.returncode == 0, f'{title}: {result.stderr}'
        else:
            assert result.returncode != 0 and error in result.stderr, f'{title}: expected {error!r}, got {result.stderr}'
        print(f'PASS: {title}')
print(f'{len(cases)} Helm catalog checks passed.')
