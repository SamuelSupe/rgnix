import base64, hashlib, http.client, json, os, pathlib, re, select, signal, socket, socketserver, ssl, subprocess, threading, time, urllib.error, urllib.request
W = pathlib.Path('/tmp/rgnix-opt4-gateway-20260927')
NS = 'rgnix-route-index-20260927'
GROUP = '/apis/gateway.networking.k8s.io/v1'
results, processes, created = [], [], []
report = {'checks': results, 'windows': [], 'environment': {'namespace':NS, 'headless_services':True,'threads':1,'server_cpu':'10','origin_cpus':'6,7','client_cpus':'2,3','concurrency':64,'warmup_seconds':3,'window_seconds':6}}

def save(): (W/'gateway.json').write_text(json.dumps(report,indent=2)+'\n')
def wait(label, check, seconds=45):
    end=time.monotonic()+seconds; last=None
    while time.monotonic()<end:
        try:
            if check():
                results.append({'name':label,'passed':True}); print('PASS',label,flush=True); save(); return
        except Exception as e: last=str(e)
        time.sleep(.25)
    raise AssertionError(f'{label}: {last}')
def port():
    with socket.socket() as s: s.bind(('127.0.0.1',0)); return s.getsockname()[1]
class Tunnel(socketserver.BaseRequestHandler):
    def handle(self):
        with socket.create_connection(('host.orb.internal',26443),5) as upstream:
            upstream.settimeout(None)
            sockets=[self.request,upstream]
            while True:
                ready,_,_=select.select(sockets,[],[],30)
                for source in ready:
                    data=source.recv(65536)
                    if not data:return
                    (upstream if source is self.request else self.request).sendall(data)
class Server(socketserver.ThreadingTCPServer):
    daemon_threads=True
bridge=Server(('127.0.0.1',0),Tunnel)
threading.Thread(target=bridge.serve_forever,daemon=True).start()
config=json.loads((W/'kubeconfig.json').read_text())
cluster=config['clusters'][0]['cluster']; user=config['users'][0]['user']
cluster['server']=f'https://127.0.0.1:{bridge.server_address[1]}'
private=W/'gateway-kubeconfig.json'; private.write_text(json.dumps(config)); private.chmod(0o600)
context=ssl.create_default_context(cadata=base64.b64decode(cluster['certificate-authority-data']).decode())
for field in ['client-certificate','client-key']:
    p=W/field; p.write_bytes(base64.b64decode(user[field+'-data'])); p.chmod(0o600)
context.load_cert_chain(W/'client-certificate',W/'client-key')
client=urllib.request.build_opener(urllib.request.ProxyHandler({}),urllib.request.HTTPSHandler(context=context))
def api(method,path,value=None):
    request=urllib.request.Request(cluster['server']+path,data=None if value is None else json.dumps(value).encode(),method=method,headers={'Content-Type':'application/json'})
    with client.open(request,timeout=15) as response: return json.load(response)
def create(path,value):
    result=api('POST',path,value); created.append(path+'/'+result['metadata']['name']); return result

def route(name,path='/api',match=None,host='api.example.test',namespace=NS,backend='a'):
    return {'apiVersion':'gateway.networking.k8s.io/v1','kind':'HTTPRoute','metadata':{'name':name,'namespace':namespace,'annotations':{'rgnix.io/access-log':'off'}},'spec':{'parentRefs':[{'name':'gateway','namespace':NS}],'hostnames':[host], 'rules':[{'matches':[match or {'path':{'type':'PathPrefix','value':path}}], 'backendRefs':[{'name':backend,'port':80}], 'filters':[{'type':'ResponseHeaderModifier','responseHeaderModifier':{'set':[{'name':'x-route','value':name}]}}]}]}}
def route_path(namespace=NS):return GROUP+f'/namespaces/{namespace}/httproutes'
def replace(path,obj):
    obj=json.loads(json.dumps(obj)); obj['metadata']['resourceVersion']=api('GET',path)['metadata']['resourceVersion']; return api('PUT',path,obj)
def request(path='/api',host='api.example.test',method='GET',headers=None,admin=False):
    connection=http.client.HTTPConnection('127.0.0.1',admin_port if admin else http_port,timeout=3)
    connection.request(method,path,headers={'Host':host,**(headers or {})}); response=connection.getresponse(); body=response.read(); out=(response.status,dict((k.lower(),v) for k,v in response.getheaders()),body); connection.close(); return out

def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:process.wait(10)
        except subprocess.TimeoutExpired:process.kill(); process.wait()

def start(binary,label):
    log=(W/f'gateway-{label}.log').open('w')
    command=['taskset','-c','10',str(binary),'gateway','--gateway',NS+'/gateway','--publish-service',NS+'/publish','--watch-namespace',NS,'--watch-namespace',NS+'-peer','--http-listen',f'127.0.0.1:{http_port}','--https-listen',f'127.0.0.1:{tls_port}','--admin',f'127.0.0.1:{admin_port}','--threads','1','--identity','route-index-benchmark','--shutdown-grace-seconds','0','--shutdown-timeout-seconds','2','--admin-users-file',str(W/'users.json')]
    process=subprocess.Popen(command,stdout=log,stderr=subprocess.STDOUT,env={**os.environ,'KUBECONFIG':str(private)}); processes.append(process)
    wait(label+' readiness',lambda:request('/readyz',admin=True)[0]==200)
    return process

def cpu(process):
    fields=pathlib.Path(f'/proc/{process.pid}/stat').read_text().rsplit(')',1)[1].split(); return (int(fields[11])+int(fields[12]))/os.sysconf('SC_CLK_TCK')
def bench(binary,label,count,round_):
    process=start(binary,f'{count}-{round_}-{label}')
    for i in (0,count//2,count-1):
        assert request(f'/api/{i}')[1].get('x-route')==f'scale-{i//16:04}'
    path=f'http://127.0.0.1:{http_port}/api/{count-1}/item?ignored=a%20b'
    cmd=['taskset','-c','2,3','wrk','-t2','-c64','-H','Host: api.example.test','--latency']
    subprocess.run(cmd+['-d3s',path],capture_output=True,check=True)
    pss_before=int(re.search(r'^Pss:\s+(\d+)',pathlib.Path(f'/proc/{process.pid}/smaps_rollup').read_text(),re.M)[1])
    before=cpu(process); start_time=time.monotonic()
    output=subprocess.run(cmd+['-d6s',path],capture_output=True,text=True,check=True).stdout
    seconds=time.monotonic()-start_time; used=cpu(process)-before
    pss_after=int(re.search(r'^Pss:\s+(\d+)',pathlib.Path(f'/proc/{process.pid}/smaps_rollup').read_text(),re.M)[1])
    errors=[line for line in output.splitlines() if 'errors:' in line or 'Non-2xx' in line]
    requests=int(re.search(r'(\d+) requests in',output)[1]); rps=float(re.search(r'Requests/sec:\s+([\d.]+)',output)[1])
    report['windows'].append({'routes':count,'round':round_,'engine':label,'sha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),'requests':requests,'rps':rps,'cpu_us_per_request':used/requests*1e6,'seconds':seconds,'pss_kib_before':pss_before,'pss_kib_after':pss_after,'errors':errors,'wrk':output});save(); print('WINDOW',count,round_,label,rps,flush=True)
    stop(process)
    assert not errors,errors

http_port,tls_port,admin_port,origin_port,origin_b=port(),port(),port(),port(),port()
address=socket.gethostbyname(socket.gethostname())
# Use the VM's routable address; Kubernetes EndpointSlice rejects loopback addresses.
address=subprocess.check_output(['hostname','-I'],text=True).split()[0]
try:
    for ns in (NS,NS+'-peer'):
        create('/api/v1/namespaces',{'apiVersion':'v1','kind':'Namespace','metadata':{'name':ns,'labels':{'rgnix-validation':NS}}})
    create(GROUP+'/gatewayclasses',{'apiVersion':'gateway.networking.k8s.io/v1','kind':'GatewayClass','metadata':{'name':NS},'spec':{'controllerName':'rgnix.io/gateway-controller'}})
    create(GROUP+f'/namespaces/{NS}/gateways',{'apiVersion':'gateway.networking.k8s.io/v1','kind':'Gateway','metadata':{'name':'gateway','namespace':NS},'spec':{'gatewayClassName':NS,'listeners':[{'name':'http','protocol':'HTTP','port':80,'allowedRoutes':{'namespaces':{'from':'All'}}}]}})
    for ns in (NS,NS+'-peer'):
        for name,up in [('a',origin_port),('b',origin_b),('publish',origin_port)]:
            create(f'/api/v1/namespaces/{ns}/services',{'apiVersion':'v1','kind':'Service','metadata':{'name':name,'namespace':ns},'spec':{'clusterIP':'None','ports':[{'name':'http','port':80,'targetPort':up}]}})
            if name!='publish':create(f'/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices',{'apiVersion':'discovery.k8s.io/v1','kind':'EndpointSlice','metadata':{'name':name,'namespace':ns,'labels':{'kubernetes.io/service-name':name}},'addressType':'IPv4','ports':[{'name':'http','port':up,'protocol':'TCP'}],'endpoints':[{'addresses':[address],'conditions':{'ready':True}}]})
    nginx=W/'origin.conf'; nginx.write_text(f'worker_processes 2; pid {W}/origin.pid; error_log {W}/origin-error.log; events {{ worker_connections 4096; }} http {{ access_log off; server {{ listen {origin_port}; location / {{ return 200 "a\\n"; }} }} server {{ listen {origin_b}; location / {{ return 200 "b\\n"; }} }} }}')
    origin=subprocess.Popen(['taskset','-c','6,7','/tmp/rgnix-compare-20260927/nginx/sbin/nginx','-c',str(nginx),'-g','daemon off;'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);processes.append(origin)
    token='gateway-index-reader-token-for-test-20260927'
    (W/'users.json').write_text(json.dumps({'users':[{'name':'reader','role':'reader','namespaces':[NS],'token_sha256':hashlib.sha256(token.encode()).hexdigest()}]}))
    initial=[route('prefix'),route('exact',match={'path':{'type':'Exact','value':'/api'}}),route('predicate',match={'path':{'type':'Exact','value':'/api'},'method':'POST','headers':[{'name':'X-Canary','value':'1'}],'queryParams':[{'name':'v','value':'a b'}]}),route('wildcard',host='*.example.test'),route('scoped-get',path='/',match={'method':'GET'},host='scope.example.test'),route('scoped-post',path='/',match={'method':'POST'},host='scope.example.test',namespace=NS+'-peer',backend='b')]
    for obj in initial:create(route_path(obj['metadata']['namespace']),obj)
    binary=W/'rgnix-candidate'; baseline=pathlib.Path('/tmp/rgnix-opt3-20260927/rgnix-candidate')
    process=start(binary,'functional')
    for label,path,host,method,headers,expected in [
        ('exact path','/api','api.example.test','GET',{},'exact'),
        ('prefix child','/api/child','api.example.test','GET',{},'prefix'),
        ('wildcard multi-label','/api','a.b.example.test','GET',{},'wildcard'),
        ('uppercase Host','/api','API.EXAMPLE.TEST','GET',{},'exact'),
        ('decoded query first value','/api?v=a+b&v=no','api.example.test','POST',{'X-Canary':'1'},'predicate'),
        ('duplicate query first value differs','/api?v=no&v=a+b','api.example.test','POST',{'X-Canary':'1'},'exact'),
        ('percent encoded query','/api?%76=a%20b','api.example.test','POST',{'X-Canary':'1'},'predicate')]:
        wait(label,lambda p=path,h=host,m=method,hs=headers,e=expected:request(p,h,m,hs)[1].get('x-route')==e)
    wait('path segment boundary',lambda:request('/apix')[0]==404)
    wait('wildcard excludes apex',lambda:request('/api',host='example.test')[0]==404)
    def simulate(method):
        connection=http.client.HTTPConnection('127.0.0.1',admin_port,timeout=5);connection.request('POST','/v1/simulate',json.dumps({'host':'scope.example.test','path':'/','method':method}),{'Authorization':'Bearer '+token,'Content-Type':'application/json'});response=connection.getresponse();response.read();status=response.status;connection.close();return status
    wait('tenant reader allowed GET simulation',lambda:simulate('GET')==200)
    wait('tenant reader denied foreign POST simulation',lambda:simulate('POST')==403)
    ep=f'/apis/discovery.k8s.io/v1/namespaces/{NS}/endpointslices/a'; endpoints=api('GET',ep); endpoints['endpoints'][0]['conditions']['ready']=False;replace(ep,endpoints)
    wait('endpoint withdrawal is live',lambda:request('/api')[0]==503)
    endpoints['endpoints'][0]['conditions']['ready']=True;replace(ep,endpoints)
    wait('endpoint restoration is live',lambda:request('/api')[0]==200)
    changed=route('prefix',path='/moved');replace(route_path()+'/prefix',changed)
    wait('route update publishes new index',lambda:request('/moved/child')[1].get('x-route')=='prefix' and request('/api/child')[1].get('x-route')=='wildcard')
    api('DELETE',route_path()+'/prefix',{})
    wait('route deletion withdraws indexed path',lambda:request('/moved/child')[0]==404)
    stop(process)
    for obj in initial:
        try:api('DELETE',route_path(obj['metadata']['namespace'])+'/'+obj['metadata']['name'],{})
        except urllib.error.HTTPError as e:
            if e.code!=404:raise
    for count in (1,1000):
        for start_ in range(0,count,16):
            obj=route(f'scale-{start_//16:04}',path=f'/api/{start_}')
            rule=obj['spec']['rules'][0]; obj['spec']['rules']=[{**rule,'matches':[{'path':{'type':'PathPrefix','value':f'/api/{i}'}}]} for i in range(start_,min(start_+16,count))]
            path=route_path()+'/'+obj['metadata']['name']
            if start_==0 and count>1:replace(path,obj)
            else:create(route_path(),obj)
        if count==1:
            for round_ in range(1,4):
                for label in (['aa-a','aa-b'] if round_%2 else ['aa-b','aa-a']):bench(binary,label,count,round_)
        for round_ in range(1,4):
            order=[('linear',baseline),('indexed',binary)]
            if round_%2==0:order.reverse()
            for label,build in order:bench(build,label,count,round_)
    report['status']='passed'
except Exception as error:
    report['status']='failed'; report['error']=str(error); raise
finally:
    for process in reversed(processes):stop(process)
    report['cleanup']=[]
    for path in [GROUP+'/gatewayclasses/'+NS, '/api/v1/namespaces/'+NS+'-peer','/api/v1/namespaces/'+NS]:
        if path not in created:continue
        try:api('DELETE',path,{});report['cleanup'].append({'path':path,'deleted':True})
        except Exception as error:report['cleanup'].append({'path':path,'error':str(error)})
    for name in ('gateway-kubeconfig.json','client-certificate','client-key','kubeconfig.json'):(W/name).unlink(missing_ok=True)
    save();bridge.shutdown()
