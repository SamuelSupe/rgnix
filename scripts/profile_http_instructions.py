#!/usr/bin/env python3
"""Count user-space HTTP/1 instructions with Cachegrind and matched startup controls."""
import argparse
from collections import Counter
import hashlib
import http.client
import json
from pathlib import Path
import platform
import statistics
import subprocess
import time
from types import SimpleNamespace

from benchmark_compare import fixtures, start, stop
from integration import free_port, request, wait_for


def profile_counts(path):
    functions = Counter()
    function = None
    summary = None
    events = None
    for line in path.read_text().splitlines():
        if line.startswith('events:'):
            events = line.split()[1:]
        elif line.startswith('summary:'):
            summary = int(line.split()[1])
        elif line.startswith('fn='):
            function = line[3:]
        elif line and line[0].isdigit():
            position, count = line.split()
            assert function is not None and position.isdecimal(), line
            functions[function] += int(count)
    assert events == ['Ir'], events
    assert summary is not None and summary == sum(functions.values()), (summary, sum(functions.values()))
    return summary, functions


def measure(binary, directory, fixture, ports, requests, warmup, cpu, headers, nginx=False, case="proxy-1k"):
    directory.mkdir()
    profile = directory / 'cachegrind.out'
    command = ['taskset', '-c', cpu, 'valgrind', '--tool=cachegrind', '--cache-sim=no', '--branch-sim=no',
               '--cachegrind-out-file=' + str(profile), str(binary)]
    command += (['-p', str(fixture / 'nginx'), '-c', str(fixture / 'nginx/nginx.conf'),
                 '-g', 'daemon off; master_process off;'] if nginx else
               ['serve', '-c', str(fixture / 'rgnix/nginx.conf'),
               '--engine', 'hyper', '--threads', '1', '--admin', f"127.0.0.1:{ports['admin']}",
               '--max-inflight', '4096', '--upstream-keepalive-pool-size', '128',
               '--shutdown-grace-seconds', '0', '--shutdown-timeout-seconds', '5'])
    server = connection = None
    try:
        server = start(command, directory / 'server.log')
        wait_for(lambda: request(ports['http'] if nginx else ports['admin'],
                                 '/' + case if nginx else '/readyz')[0], 200, timeout=60)
        connection = http.client.HTTPConnection('127.0.0.1', ports['http'], timeout=30)
        started = time.monotonic()
        for _ in range(warmup + requests):
            connection.request('GET', '/' + case, headers=headers)
            response = connection.getresponse()
            assert response.status == 200 and response.read() == b'x' * (1024 if case == 'proxy-1k' else 16384)
        seconds = time.monotonic() - started
        connection.close()
        connection = None
    finally:
        if connection:
            connection.close()
        stop(server)
    total, functions = profile_counts(profile)
    return {'requests': requests, 'warmup_requests': warmup, 'instructions': total, 'functions': dict(functions),
            'seconds_including_warmup': seconds, 'command': command, 'profile': str(profile),
            'profile_sha256': hashlib.sha256(profile.read_bytes()).hexdigest()}


def summarize(records):
    result = {}
    for engine in dict.fromkeys(r['engine'] for r in records):
        runs = [r for r in records if r['engine'] == engine]
        values = [r['instructions_per_request'] for r in runs]
        differences = [200 * abs(a['instructions_per_request'] - b['instructions_per_request']) /
                       (a['instructions_per_request'] + b['instructions_per_request'])
                       for a, b in zip(runs[::2], runs[1::2])]
        cv = 100 * statistics.stdev(values) / statistics.mean(values)
        result[engine] = {'median_instructions_per_request': statistics.median(values), 'cv_percent': cv,
                          'aa_pair_symmetric_differences_percent': differences,
                          'aa_gate_1_percent': len(differences) >= 3 and max(differences) <= 1 and cv <= 1}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--candidate', type=Path)
    parser.add_argument('--nginx', type=Path, help='optional one-worker NGINX control, without a forking master')
    parser.add_argument('--work-dir', type=Path, required=True, help='new directory on native Linux storage')
    parser.add_argument('--requests', type=int, default=2000)
    parser.add_argument('--warmup', type=int, default=100)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--server-cpu', default='10')
    parser.add_argument('--origin-cpu', default='6')
    parser.add_argument('--case', choices=['proxy-1k', 'proxy-16k'], default='proxy-1k')
    parser.add_argument('--extra-headers', type=int, default=0, help='fixed additional request fields for header-width sensitivity')
    args = parser.parse_args()
    if min(args.requests, args.warmup, args.rounds) < 1:
        parser.error('counts must be positive')
    if not 0 <= args.extra_headers <= 64:
        parser.error('extra headers must be 0..64')
    headers = {'Host': 'localhost', **{f'X-Extra-{n}': f'value-{n}' for n in range(args.extra_headers)}}
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    ports = {}
    for name in ['primary', 'canary', 'http', 'tls', 'admin']:
        port = free_port()
        while port in ports.values():
            port = free_port()
        ports[name] = port
    binaries = {'baseline': args.binary.resolve()}
    if args.candidate:
        binaries['candidate'] = args.candidate.resolve()
    if args.nginx:
        binaries['nginx'] = args.nginx.resolve()
    fixture = work / 'fixture'
    fixture.mkdir()
    fixtures(SimpleNamespace(plain_proxy=True, workers=1, origin_workers=1, cases=[args.case], connection_close=False), fixture, ports)
    output = {'method': f'Single worker, one persistent client, validated {args.case} responses. Each sample subtracts its own full startup/warmup/shutdown control from a full run with N extra requests. Flat user-space instructions only; kernel work, cache contention, throughput and P99 are not measured. All samples retained.',
              'platform': platform.platform(), 'valgrind': subprocess.check_output(['valgrind', '--version'], text=True).strip(),
              'settings': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              'binaries': {name: {'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()} for name, path in binaries.items()},
              'fixtures': {str(p.relative_to(fixture)): p.read_text() for p in fixture.rglob('*') if p.suffix in ['.conf', '.lua', '.rgl']},
              'records': [], 'started_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}
    destination = work / 'result.json'
    origin = None
    try:
        directory = fixture / 'origin'
        origin = start(['taskset', '-c', args.origin_cpu, '/usr/sbin/nginx', '-p', str(directory), '-c', str(directory / 'nginx.conf'), '-g', 'daemon off;'], work / 'origin.log')
        wait_for(lambda: request(ports['primary'], '/proxy-1k')[0], 200)
        for round_index in range(args.rounds):
            order = list(binaries) if round_index % 2 == 0 else list(reversed(binaries))
            for repeat in range(2):
                for engine in order:
                    runs = {}
                    for count in ([0, args.requests] if (round_index + repeat) % 2 == 0 else [args.requests, 0]):
                        runs[count] = measure(binaries[engine], work / f'{round_index + 1}-{repeat + 1}-{engine}-{count}', fixture, ports, count, args.warmup, args.server_cpu, headers, engine == 'nginx', args.case)
                    load, control = runs[args.requests], runs[0]
                    difference = Counter(load['functions'])
                    difference.subtract(control['functions'])
                    net = load['instructions'] - control['instructions']
                    assert net > 0 and sum(difference.values()) == net
                    output['records'].append({'engine': engine, 'round': round_index + 1, 'repeat': repeat + 1,
                        'instructions_per_request': net / args.requests, 'load': load, 'control': control,
                        'function_delta': dict(difference)})
                    destination.write_text(json.dumps(output, indent=2) + '\n')
                    print(engine, round_index + 1, repeat + 1, round(net / args.requests, 2), 'Ir/request', flush=True)
        output['summary'] = summarize(output['records'])
        output['completed_at'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    finally:
        stop(origin)
        destination.write_text(json.dumps(output, indent=2) + '\n')
    print(json.dumps(output['summary'], indent=2))


if __name__ == '__main__':
    main()
