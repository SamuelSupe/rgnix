#!/usr/bin/env python3
"""Repeat a bounded, closed-loop HTTP capacity matrix inside a dedicated test cluster."""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import time

from benchmark_compare import REPORT_LUA


CLIENT = r'''import json,os,pathlib,resource,subprocess,sys,time,urllib.request
config=json.loads(sys.argv[1])
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
def snapshot():
 result={}
 for name,address in config['addresses'].items():
  text=opener.open('http://'+address+':9090/metrics',timeout=3).read().decode()
  result[name]={line.rsplit(None,1)[0]:float(line.rsplit(None,1)[1]) for line in text.splitlines()
   if line.startswith(('process_','rgnix_ready ','rgnix_hyper_','rgnix_budget_','rgnix_requests_total','rgnix_rejected_requests_total','rgnix_engine_info'))}
  assert result[name].get('rgnix_ready')==1, result[name]
  assert result[name].get('rgnix_engine_info{engine="hyper"}')==1, result[name]
 return result
urls=[config['url']]+['http://'+address+':8080/'+config['url'].rsplit('/',1)[1] for address in config['addresses'].values()]
for url in urls:
 request=urllib.request.Request(url,headers={'Host':'capacity.example.test'})
 with opener.open(request,timeout=5) as response:
  assert response.status==200 and response.read()==b'x'*config['size'], 'payload preflight failed'
argv=['wrk','-t','4','-c',str(config['concurrency']),'-d',str(config['seconds'])+'s','--timeout','5s','--latency','-s','/fixture/report.lua','-H','Host: capacity.example.test',config['url']]
before=snapshot(); usage=resource.getrusage(resource.RUSAGE_CHILDREN)
cpu_before=pathlib.Path('/sys/fs/cgroup/cpu.stat').read_text()
load_before=pathlib.Path('/proc/loadavg').read_text()
samples=[]; started=time.monotonic()
process=subprocess.Popen(argv,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
try:
 while process.poll() is None:
  assert time.monotonic()-started<config['seconds']+20, 'load exceeded its deadline'
  samples.append({'elapsed_seconds':time.monotonic()-started,'pods':snapshot()})
  time.sleep(1)
 output=process.communicate(timeout=5)[0]
 assert process.returncode==0, output
finally:
 if process.poll() is None: process.kill(); process.wait()
after=snapshot(); end_usage=resource.getrusage(resource.RUSAGE_CHILDREN)
result=json.loads(next(line.removeprefix('BENCH_RESULT ') for line in output.splitlines() if line.startswith('BENCH_RESULT ')))
result.update(command=argv,wrk_output=output,before=before,after=after,samples=samples,
 client_cpu_cores=(end_usage.ru_utime+end_usage.ru_stime-usage.ru_utime-usage.ru_stime)/result['seconds'],
 client_cgroup_before=cpu_before,client_cgroup_after=pathlib.Path('/sys/fs/cgroup/cpu.stat').read_text(),
 load_before=load_before,load_after=pathlib.Path('/proc/loadavg').read_text(),client_cpu_affinity=sorted(os.sched_getaffinity(0)),
 client_memory_current_bytes=int(pathlib.Path('/sys/fs/cgroup/memory.current').read_text()))
result['errors']=sum(value for key,value in result.items() if key.endswith('_errors'))
result['rps']=result['requests']/result['seconds']
result['error_fraction']=result['errors']/max(1,result['requests'])
print(json.dumps(result),flush=True)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--image', required=True)
    parser.add_argument('--fixture-image', required=True, help='image containing Python 3, wrk and NGINX')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--seconds', type=int, default=30)
    parser.add_argument('--warmup', type=int, default=5)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--concurrency', type=int, nargs='+', default=[16, 64, 256, 1024])
    args = parser.parse_args()
    assert 1 <= args.seconds <= 300 and 1 <= args.warmup <= 60 and 2 <= args.rounds <= 10
    assert all(4 <= value <= 4096 for value in args.concurrency)
    ns = args.namespace
    kubectl = ['kubectl', '--context', args.context]
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def command(*argv, data=None, timeout=180):
        result = subprocess.run(argv, input=data, text=True, capture_output=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(result.stderr or result.stdout)
        return result.stdout

    def apply(*items):
        command(*kubectl, 'apply', '-f', '-', data=json.dumps({'apiVersion':'v1','kind':'List','items':items}))

    def get(kind, *extra):
        return json.loads(command(*kubectl, '-n', ns, 'get', kind, *extra, '-o', 'json'))

    existing = command(*kubectl, 'get', 'namespace', ns, '--ignore-not-found', '-o', 'json')
    if existing and json.loads(existing).get('metadata', {}).get('labels', {}).get('capacity-qa') != ns:
        raise RuntimeError('refusing to reuse a namespace not owned by this fixture')
    nodes = get('nodes')['items']
    control = next(node['metadata']['name'] for node in nodes if 'node-role.kubernetes.io/control-plane' in node['metadata']['labels'])
    assert len(nodes) >= 3, 'requires one control-plane and two worker nodes'
    apply({'apiVersion':'v1','kind':'Namespace','metadata':{'name':ns,'labels':{'capacity-qa':ns}}})
    config = 'worker_processes 4; pid /tmp/nginx.pid; error_log /tmp/error.log warn; events { worker_connections 8192; } http { access_log off; sendfile on; keepalive_timeout 60; client_body_temp_path /tmp/client; proxy_temp_path /tmp/proxy; fastcgi_temp_path /tmp/fastcgi; uwsgi_temp_path /tmp/uwsgi; scgi_temp_path /tmp/scgi; server { listen 8080; root /fixture; } }'
    apply({'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'fixture','namespace':ns},'data':{
        'nginx.conf':config,'proxy-1k':'x'*1024,'proxy-16k':'x'*16384,'report.lua':REPORT_LUA,'client.py':CLIENT}})
    for name in ['origin', 'client']:
        container = {'name':name,'image':args.fixture_image,'imagePullPolicy':'IfNotPresent',
            'command':['nginx','-c','/fixture/nginx.conf','-g','daemon off;'] if name == 'origin' else ['sleep','infinity'],
            'resources':{'requests':{'cpu':'100m','memory':'64Mi'},'limits':{'cpu':'4','memory':'1Gi'}},
            'volumeMounts':[{'name':'fixture','mountPath':'/fixture','readOnly':True}]}
        apply({'apiVersion':'apps/v1','kind':'Deployment','metadata':{'name':name,'namespace':ns},'spec':{
            'replicas':1,'selector':{'matchLabels':{'capacity-fixture':name}},'template':{'metadata':{'labels':{'capacity-fixture':name}},'spec':{
                'nodeSelector':{'kubernetes.io/hostname':control},'tolerations':[{'key':'node-role.kubernetes.io/control-plane','operator':'Exists','effect':'NoSchedule'}],
                'containers':[container],'volumes':[{'name':'fixture','configMap':{'name':'fixture'}}]}}}})
        command(*kubectl, '-n', ns, 'rollout', 'status', 'deployment/'+name, '--timeout=150s')
    apply({'apiVersion':'v1','kind':'Service','metadata':{'name':'origin','namespace':ns},'spec':{'selector':{'capacity-fixture':'origin'},'ports':[{'port':80,'targetPort':8080}]}})
    repository, tag = args.image.rsplit(':', 1)
    values = {'mode':'gateway','gateway':{'name':'gateway','className':ns,'listeners':[{'name':'http','protocol':'HTTP','port':80}]},
        'image':{'repository':repository,'tag':tag},'replicaCount':2,'requireMultipleNodes':True,'watchNamespaces':[ns],
        'service':{'type':'ClusterIP'},'engine':'hyper','threads':2,'reportReplicas':True,'shutdown':{'enabled':True},
        'maxInflight':4096,'upstreamKeepalivePoolSize':128,'upstreamMaxFails':0,
        'experimentalHyper':{'maxConnections':16384,'maxConnectionsPerIp':16384},
        'resources':{'requests':{'cpu':'100m','memory':'128Mi'},'limits':{'cpu':'2','memory':'512Mi'}}}
    values_file = args.output.with_suffix('.values.json')
    values_file.write_text(json.dumps(values, indent=2)+'\n')
    command('helm','--kube-context',args.context,'upgrade','--install','capacity',str(Path(__file__).resolve().parents[1]/'charts/rgnix'),'-n',ns,'-f',str(values_file),'--wait','--timeout','150s')
    routes = []
    for index in range(201):
        routes.append({'apiVersion':'gateway.networking.k8s.io/v1','kind':'HTTPRoute','metadata':{'name':f'capacity-{index}','namespace':ns},'spec':{
            'parentRefs':[{'name':'gateway'}],'hostnames':['capacity.example.test' if index == 0 else f'scale-{index}.example.test'],
            'rules':[{'backendRefs':[{'name':'origin','port':80}]}]}})
    apply(*routes)
    deadline = time.monotonic()+120
    while True:
        states = get('httproutes')['items']
        if len(states)==201 and all(any(c['type']=='ResolvedRefs' and c['status']=='True' and c.get('observedGeneration')==state['metadata']['generation']
                for parent in state.get('status',{}).get('parents',[]) for c in parent.get('conditions',[])) for state in states):
            break
        assert time.monotonic()<deadline, 'route did not converge'
        time.sleep(1)
    pods = get('pods','-l','app.kubernetes.io/instance=capacity')['items']
    assert len(pods)==2 and len({p['spec']['nodeName'] for p in pods})==2
    identities = {p['metadata']['name']:p['metadata']['uid'] for p in pods}
    addresses = {p['metadata']['name']:p['status']['podIP'] for p in pods}
    output = {'started_at':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'settings':{k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()},
        'topology':{'nodes':nodes,'pods':pods,'fixtures':get('pods','-l','capacity-fixture')['items']},'values':values,'runs':[], 'complete':False,
        'scope':'closed-loop HTTP/1 keepalive through ClusterIP; 1/16 KiB payloads; no TLS, RGL or OTLP; wrk corrected latency; shared physical host'}

    def save():
        args.output.write_text(json.dumps(output, indent=2)+'\n')

    def cgroups():
        return {name:command(*kubectl,'-n',ns,'exec',name,'--','cat','/sys/fs/cgroup/cpu.stat') for name in [*addresses,'deployment/origin']}

    try:
        versions = command(*kubectl,'-n',ns,'exec','deployment/client','--','sh','-c','wrk --version; nginx -v 2>&1; python3 --version')
        output['fixture_versions'] = versions
        windows = [(concurrency,size) for concurrency in args.concurrency for size in [1024,16384]]
        # The same image and deployment remain in place for all A/A repetitions.
        for round_index in range(args.rounds):
            ordered = windows if round_index % 2 == 0 else list(reversed(windows))
            if round_index in (0,args.rounds-1):
                ordered = [('origin',1024),('origin',16384)]+ordered
            for concurrency,size in ordered:
                origin = concurrency=='origin'
                actual_concurrency = max(args.concurrency) if origin else concurrency
                path = 'proxy-1k' if size==1024 else 'proxy-16k'
                parameters = {'url':f'http://{"origin" if origin else "capacity"}.{ns}.svc:80/{path}',
                    'addresses':{} if origin else addresses,'concurrency':actual_concurrency,'size':size}
                for phase,seconds in [('warmup',args.warmup),('measure',args.seconds)]:
                    output['current_window'] = {'round':round_index+1,'phase':phase,'origin':origin,'concurrency':actual_concurrency,'size':size}
                    save()
                    before = cgroups()
                    raw = command(*kubectl,'-n',ns,'exec','deployment/client','--','python3','/fixture/client.py',json.dumps({**parameters,'seconds':seconds}),timeout=seconds+45)
                    run = json.loads(raw)
                    run.update(round=round_index+1,phase=phase,origin=origin,concurrency=actual_concurrency,size=size,cgroup_before=before,cgroup_after=cgroups())
                    run['slo_pass'] = run['requests']>0 and run['error_fraction']<=.001 and run['p99_ms']<=50
                    output['runs'].append(run); save()
                    print(f'{phase} round={round_index+1} origin={origin} size={size} c={actual_concurrency}: {run["rps"]:.0f} rps, p99={run["p99_ms"]:.3f}ms, errors={run["errors"]}',flush=True)
                current = get('pods','-l','app.kubernetes.io/instance=capacity')['items']
                assert {p['metadata']['name']:p['metadata']['uid'] for p in current}==identities, 'gateway pod identity changed during measurement'
                assert all(c.get('restartCount',0)==0 for p in current for c in p['status']['containerStatuses']), 'gateway restarted'
        output['summary'] = {}
        for concurrency,size in windows:
            runs = [r for r in output['runs'] if r['phase']=='measure' and not r['origin'] and r['concurrency']==concurrency and r['size']==size]
            summary = {'requests':sum(r['requests'] for r in runs),'errors':sum(r['errors'] for r in runs),'all_windows_slo_pass':all(r['slo_pass'] for r in runs)}
            for field in ['rps','p50_ms','p95_ms','p99_ms','client_cpu_cores']:
                values = [r[field] for r in runs]
                summary[field] = {'min':min(values),'median':statistics.median(values),'max':max(values),'cv_pct':statistics.stdev(values)/statistics.mean(values)*100 if statistics.mean(values) else 0}
            summary['repeatability_pass'] = all(summary[field]['median']>0 and summary[field]['cv_pct']<=10 and (summary[field]['max']-summary[field]['min'])/summary[field]['median']<=.1 for field in ['rps','p99_ms'])
            output['summary'][f'{size}/c{concurrency}'] = summary
        output.pop('current_window', None)
        output['complete'] = True
    except BaseException as error:
        output['failure'] = {'type':type(error).__name__,'message':str(error)}
        raise
    finally:
        save()
    print('Complete; test namespace retained for inspection',flush=True)


if __name__ == '__main__':
    main()
