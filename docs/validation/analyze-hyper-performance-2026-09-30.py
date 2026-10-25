"""Recompute the recorded A/A criteria and medians from a benchmark_compare JSON."""
import json
import statistics
import sys
from pathlib import Path

path = Path(sys.argv[1])
matrix = json.loads(path.read_text())
groups = {}
for run in matrix['runs']:
    key = (run['engine'], run['case'], run['concurrency'])
    groups.setdefault(key, []).append(run)
output = {'criteria': {'max_pair_symmetric_difference_pct': 10, 'max_group_cv_pct': 10, 'errors': 0}, 'groups': {}, 'comparisons': {}}
for (engine, case, concurrency), runs in groups.items():
    rounds = {}
    for run in runs:
        rounds.setdefault(run['round'], []).append(run)
    pairs = [values for values in rounds.values() if len(values) == 2]
    differences = [abs(pair[0]['rps'] - pair[1]['rps']) / statistics.mean(r['rps'] for r in pair) * 100 for pair in pairs]
    columns = [[pair[index]['rps'] for pair in pairs] for index in range(2)]
    cvs = [statistics.stdev(values) / statistics.mean(values) * 100 if len(values) > 1 else None for values in columns]
    output['groups'][f'{engine}/{case}/c{concurrency}'] = {
        'windows': len(runs), 'requests': sum(r['requests'] for r in runs),
        'errors': sum(r['errors'] for r in runs),
        'rps_median': statistics.median(r['rps'] for r in runs),
        'cpu_us_median': statistics.median(r['server_cpu_us_per_request'] for r in runs),
        'p99_ms_max': max(r['p99_ms'] for r in runs),
        'peak_pss_mib': max(r['peak_pss_kib'] for r in runs) / 1024,
        'aa_pair_difference_pct': differences, 'aa_group_cv_pct': cvs,
        'aa_passed': len(pairs) == matrix['settings']['rounds'] and len(pairs) >= 3
                     and all(r['errors'] == 0 for r in runs)
                     and all(value <= 10 for value in differences)
                     and all(value is not None and value <= 10 for value in cvs),
    }
for case in matrix['settings']['cases']:
    for concurrency in matrix['settings']['concurrency']:
        keys = {engine: f'{engine}/{case}/c{concurrency}' for engine in ['rgnix', 'candidate', 'nginx']}
        if not all(key in output['groups'] for key in keys.values()):
            continue
        values = {engine: output['groups'][key] for engine, key in keys.items()}
        output['comparisons'][f'{case}/c{concurrency}'] = {
            'candidate_vs_before_rps_pct': (values['candidate']['rps_median'] / values['rgnix']['rps_median'] - 1) * 100,
            'candidate_vs_before_cpu_pct': (values['candidate']['cpu_us_median'] / values['rgnix']['cpu_us_median'] - 1) * 100,
            'candidate_over_nginx_rps': values['candidate']['rps_median'] / values['nginx']['rps_median'],
            'before_and_candidate_calibrated': values['rgnix']['aa_passed'] and values['candidate']['aa_passed'],
            'candidate_and_nginx_calibrated': values['candidate']['aa_passed'] and values['nginx']['aa_passed'],
        }
destination = path.with_name(path.stem + '-analysis.json')
destination.write_text(json.dumps(output, indent=2) + '\n')
print(destination)
for key, value in output['groups'].items():
    print(key, 'RPS', round(value['rps_median']), 'CPU us', round(value['cpu_us_median'], 2), 'A/A', value['aa_passed'])
