"""Two-project REST and native-client contract on an isolated Podman stack."""
import concurrent.futures
import http.client
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.parse

session, phase = sys.argv[1:]
state = pathlib.Path('.state') / session
admin = (state / 'admin-api-key').read_text().strip()

def base(port):
    address = subprocess.check_output(['podman', 'port', f'vertex-shim-{session}-proxy', f'{port}/tcp'], text=True).strip()
    return ('127.0.0.1', int(address.rsplit(':', 1)[1]))

ports = {p: base(p) for p in (8080, 8081)}

def call(path, key=admin, body=None, method=None, port=8081, api_key=False):
    c = http.client.HTTPConnection(*ports[port], timeout=15)
    try:
        headers = {'x-api-key': key} if api_key else {'Authorization': 'Bearer ' + key}
        headers.update({'Content-Type': 'application/json', 'anthropic-version': '2023-06-01'})
        c.request(method or ('POST' if body is not None else 'GET'), path, json.dumps(body) if body is not None else None, headers)
        r=c.getresponse(); wire=r.read()
        return r.status, json.loads(wire) if wire.startswith(b'{') else None
    finally: c.close()

def project(id, profile='default', enabled=True):
    return {'project_id':id,'display_name':id,'upstream_profile':profile,'enabled':enabled}

def message(project, operation='messages', key=None, vertex=False):
    access=key or (state / f'{project}-key').read_text().strip()
    body={'model':'claude-opus-4-6','messages':[{'role':'user','content':'project_private_prompt'}]}
    if operation != 'messages/count_tokens': body['max_tokens']=16
    if vertex:
        body.pop('model');body['anthropic_version']='vertex-2023-10-16'
        route=f'/v1/projects/agent-{project}/locations/us-east5/publishers/anthropic/models/claude-opus-4-6:rawPredict'
    else: route='/v1/'+operation
    return call(route,access,body,port=8080,api_key=not vertex)

def records(id):
    for _ in range(40):
        status,page=call('/api/private/v1/projects/'+id+'/telemetry/requests?limit=500')
        assert status==200
        if all(r['completion']!='in_progress' for r in page['items']): return page['items']
        time.sleep(.05)
    raise AssertionError('in-progress telemetry did not settle')

if phase=='provision':
    status,initial=call('/api/private/v1/projects');assert status==200 and initial['items'][0]['project_id']=='vertex-shim-local'
    assert call('/api/private/v1/projects',key='invalid')[0]==401
    for name,profile in [('alpha','default'),('beta','account-b')]:
        assert call('/api/private/v1/projects',body=project('agent-'+name,profile))[0]==201
        status,key=call(f'/api/private/v1/projects/agent-{name}/credentials',body={'client_id':name+'-agent','backends':['anthropic'],'catalog_models':True,'vertex_catalog_models':True,'mock_adc':True})
        assert status==201
        fd=os.open(state / f'{name}-key',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as f:f.write(key.pop('api_key'))
        (state / f'{name}-credential.json').write_text(json.dumps(key))
    print('PASS: provisioned distinct projects and local workload credentials')
elif phase=='exercise':
    # Native clients must have used their normal ADC path and correct project.
    for name,auth in [('alpha','bearer-file'),('beta','claude-oauth')]:
        assert 'héllo_private_response' in (state/f'{name}-native.txt').read_text()
        rows=records('agent-'+name)
        assert any(r['auth_scheme']=='mock-adc' and r['operation']=='stream_generation' and r['upstream_auth']==auth and r['http_status']==200 for r in rows)
        assert all(r['project_id']=='agent-'+name and r['client_id']==name+'-agent' for r in rows)
    # REST key schemas and counting share tenant attribution with native ADC.
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        replies=list(pool.map(lambda n:message(n),['alpha','beta']*4))
    assert all(s==200 for s,_ in replies)
    for name in ['alpha','beta']:
        assert message(name,'messages/count_tokens')[0]==200
        assert message(name,vertex=True)[0]==200
    a=(state/'alpha-key').read_text().strip()
    assert message('beta',key=a,vertex=True)[0]==403
    assert call('/api/private/v1/projects',key=a)[0]==401
    for name,mode in [('alpha','bearer-file'),('beta','claude-oauth')]:
        rows=records('agent-'+name)
        assert len(rows)>=7 and all(r['upstream_auth']==mode for r in rows)
        status,summary=call(f'/api/private/v1/projects/agent-{name}/telemetry/summary?limit=500')
        assert status==200 and summary['counting_requests']==1 and summary['generation_tokens']['output_tokens']>=18
        account='account-a' if name=='alpha' else 'account-b'
        stats=json.loads(subprocess.check_output(['podman','exec',f'vertex-shim-{session}-client','curl','-fsS',f'http://{account}:8090/stats'],text=True))
        assert stats.get('contract_failures',0)==0 and stats.get('refresh_requests',0)==0
    credential=json.loads((state/'alpha-credential.json').read_text());id=credential['key_id']
    status,replacement=call(f'/api/private/v1/projects/agent-alpha/credentials/{id}/rotate',method='POST');assert status==200
    assert message('alpha',key=a)[0]==401
    fd=os.open(state/'alpha-key',os.O_WRONLY|os.O_TRUNC)
    with os.fdopen(fd,'w') as f:f.write(replacement['api_key'])
    assert call('/api/private/v1/projects/agent-alpha',body=project('agent-alpha',enabled=False),method='PUT')[0]==200
    assert message('alpha')[0]==401 and message('beta')[0]==200
    assert call('/api/private/v1/projects/agent-alpha',body=project('agent-alpha'),method='PUT')[0]==200
    assert message('alpha')[0]==200
    print('PASS: concurrent ADC clients, REST isolation, upstream credential separation, telemetry, rotation, and disable/re-enable')
elif phase=='persistence':
    assert message('alpha')[0]==200 and message('beta')[0]==200
    for name in ['alpha','beta']: assert len(records('agent-'+name))>=8
    credential=json.loads((state/'alpha-credential.json').read_text());id=credential['key_id']
    assert call(f'/api/private/v1/projects/agent-alpha/credentials/{id}',method='DELETE')[0]==204
    assert message('alpha')[0]==401
    assert call('/api/private/v1/projects/agent-alpha',method='DELETE')[0]==204
    assert call('/api/private/v1/projects/agent-alpha')[0]==404
    assert len(records('agent-alpha'))>=8
    assert message('beta')[0]==200
    print('PASS: persisted identities/grants/usage after proxy restart; deletion retains history and leaves other tenant working')
elif phase=='privacy':
    log=(state/'proxy.log').read_text()
    for path in ['alpha-key','beta-key','admin-api-key','proxy-api-key','account-a-token','upstreams/account-b-token']:
        assert (state/path).read_text().strip() not in log
    assert 'mock-gcp-' not in log and 'project_private_prompt' not in log
    for name in ['alpha','beta']:
        serialized=json.dumps(records('agent-'+name))
        assert (state/f'{name}-key').read_text().strip() not in serialized and 'project_private_prompt' not in serialized
    print('PASS: no workload/upstream secrets, tokens, or prompts in stdout or project telemetry')
