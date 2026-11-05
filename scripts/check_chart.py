#!/usr/bin/env python3
"""Exercise Helm render-time catalog validation without accessing a cluster."""
import json
from pathlib import Path
import subprocess
import tempfile

chart = Path(__file__).resolve().parents[1] / 'chart' / 'temba-catalog'

def server(identifier, endpoint='https://mcp.example.com/mcp', **extra):
    return {'name': identifier, 'endpoint': endpoint, **extra}

cases = [
    ('empty catalog', [], None),
    ('unique names', [server('service-a'), server('service-b')], None),
    ('case-sensitive names', [server('Service'), server('service')], None),
    ('same name, different endpoint', [server('same'), server('same', 'https://other.example.com/mcp')], 'duplicate MCP name "same"'),
    ('identical entries', [server('same'), server('same')], 'duplicate MCP name "same"'),
    ('missing name', [{'endpoint': 'https://mcp.example.com/mcp'}], '.name must be a nonempty string'),
    ('numeric name', [server(123)], '.name must be a nonempty string'),
    ('empty name', [server('')], '.name is invalid'),
    ('name with spaces', [server('Service A')], '.name is invalid'),
    ('reserved local name', [server('__local_reserved')], '.name is invalid'),
    ('name too long', [server('a' * 65)], '.name is invalid'),
    ('removed id', [server('service', id='service')], '.id is no longer supported'),
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
        if error and 'duplicate MCP name' in error:
            assert 'catalog.servers[1].name' in result.stderr and 'catalog.servers[0].name' in result.stderr
        print(f'PASS: {title}')
print(f'{len(cases)} Helm catalog checks passed.')
