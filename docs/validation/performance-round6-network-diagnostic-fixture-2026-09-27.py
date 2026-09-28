import concurrent.futures,http.client,json,subprocess,sys,time
from pathlib import Path
w=Path('/tmp/rgnix-opt6-20260927');c=w/'ab-two'
p=json.loads((c/'results.json').read_text());ports=p['ports']
sys.path.insert(0,str(w/'source/scripts'))
from benchmark_compare import start,stop
from integration import request,wait_for

def counters():
 result={}
 for file in ['netstat','snmp']:
  lines=Path('/proc/net/'+file).read_text().splitlines()
  for names,values in zip(lines[::2],lines[1::2]):
   result.update({names.split()[0]+key:int(value) for key,value in zip(names.split()[1:],values.split()[1:])})
 return result

def capture():
 return {'time':time.monotonic(),'net':counters(),'sockets':subprocess.check_output(['ss','-antp',f'( sport = :{ports["http"]} or sport = :{ports["origin"]} or dport = :{ports["origin"]} )'],text=True),'processes':subprocess.check_output(['ps','-eLo','pid,tid,psr,pcpu,stat,wchan:24,comm'],text=True),'origin_error':(c/'origin-error.log').read_text()}

def probe(i):
 host=f'127.0.{i//250}.{i%250+1}';begin=time.monotonic()
 con=http.client.HTTPConnection(host,ports['origin'],timeout=2)
 try:
  con.request('GET','/',headers={'Host':'localhost'});r=con.getresponse();b=r.read()
  return {'host':host,'status':r.status,'body_bytes':len(b),'elapsed':time.monotonic()-begin}
 except Exception as e:return {'host':host,'error':repr(e),'elapsed':time.monotonic()-begin}
 finally:con.close()

origin=server=load=None
r={'qualification':'Instrumented network-stall diagnostic, not a throughput measurement','snapshots':[],'before':counters()}
try:
 origin=start(['taskset','-c','6,7',p['settings']['nginx'],'-p',str(c),'-c',str(c/'origin.conf'),'-g','daemon off;'],w/'network-origin.log')
 wait_for(lambda:request(ports['origin'],'/')[0],200)
 with concurrent.futures.ThreadPoolExecutor(max_workers=64) as ex:r['direct_before']=list(ex.map(probe,range(256)))
 print('direct before errors',sum('error' in x for x in r['direct_before']),flush=True)
 server=start(['taskset','-c','10,11',p['binaries']['before']['path'],'serve','-c',str(c/'rr-64.conf'),'--admin',f"127.0.0.1:{ports['admin']}",'--threads','2','--shutdown-grace-seconds','0','--shutdown-timeout-seconds','5'],w/'network-before.log')
 wait_for(lambda:request(ports['admin'],'/readyz')[0],200,timeout=30)
 load=subprocess.Popen(['taskset','-c','2,3','wrk','-t','2','-c','64','-d','30s','--timeout','5s','--latency','-s',str(c/'rr-64.lua'),'-H','Host: localhost',f"http://127.0.0.1:{ports['http']}/"],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
 for i in range(6):
  time.sleep(4)
  r['snapshots'].append(capture())
  with concurrent.futures.ThreadPoolExecutor(max_workers=64) as ex:rows=list(ex.map(probe,range(64)))
  r.setdefault('direct_during',[]).append(rows)
  print('sample',i,'direct errors',sum('error' in x for x in rows),'max latency',max(x['elapsed'] for x in rows),flush=True)
  (w/'network-diagnostic.json').write_text(json.dumps(r,indent=2)+'\n')
 r['wrk_stdout'],r['wrk_stderr']=load.communicate(timeout=10);load=None
 print(r['wrk_stdout'][-1200:],flush=True)
finally:
 if load is not None and load.poll() is None:load.terminate();load.wait()
 stop(server);stop(origin)
 r['after']=counters()
 r['counter_delta']={k:v-r['before'].get(k,0) for k,v in r['after'].items() if v!=r['before'].get(k,0)}
 (w/'network-diagnostic.json').write_text(json.dumps(r,indent=2)+'\n')
 print(r['counter_delta'],flush=True)
