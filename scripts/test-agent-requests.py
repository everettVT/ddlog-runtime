#!/usr/bin/env python3
"""Real MCP + DDlog test of registered lowering. External worker is a labeled mock."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

with tempfile.TemporaryDirectory(prefix='registered-agent-') as folder:
    registry=Path(folder)/'operations.json'
    registry.write_text(json.dumps({'review':{'version':'v1','description':'Review a text payload'}}))
    env=dict(os.environ,LEMMALOG_AGENT_OPERATIONS=str(registry))
    p=subprocess.Popen([os.environ.get('LEMMALOG_DDLOG_MCP','target/debug/lemmalog-ddlog-mcp')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True,env=env)
    receipts=[]
    def rpc(method,args):
        request={'jsonrpc':'2.0','id':len(receipts)+1,'method':method,'params':args}
        p.stdin.write(json.dumps(request)+'\n');p.stdin.flush()
        response=json.loads(p.stdout.readline())
        receipts.append({'request':request,'response':response})
        return response['result']
    def call(name,args,error=False):
        result=rpc('tools/call',{'name':name,'arguments':args})
        assert result['isError']==error,result
        return result if error else json.loads(result['content'][0]['text'])
    def submit(entity,revision,payload):
        return call('submit_agent_input',{'entity':entity,'revision':revision,'payload':payload})['request_id']
    def query():
        return call('lemmalog_query',{'predicate':'reviewed'})['rows']
    try:
        rpc('initialize',{})
        assert len(rpc('tools/list',{})['tools'])==10
        assert call('agent_operations',{})['operations'][0]['version']=='v1'
        args={'operation':'review','rules':'reviewed(E,R,O) :- agent_result(E,R,O).','schemas':{'reviewed':{'input':False,'fields':['string','int','string']}}}
        call('install_agent_program',dict(args,operation='unregistered'),error=True)
        call('install_agent_program',args)
        first=submit('agent-a',1,'old payload')
        assert first==submit('agent-a',1,'old payload')
        call('submit_agent_input',{'entity':'agent-a','revision':1,'payload':'conflict'},error=True)
        assert call('lemmalog_query',{'predicate':'agent_pending'})['rows']
        call('claim_agent_request',{'request_id':first})
        assert call('lemmalog_query',{'predicate':'agent_pending'})['rows']==''
        assert call('lemmalog_query',{'predicate':'agent_running'})['rows']
        call('claim_agent_request',{'request_id':first},error=True)
        second=submit('agent-a',2,'new payload')
        assert first!=second and query()==''
        # An external mock worker completes the now-stale first request.
        stale=call('complete_agent_request',{'request_id':first,'output':'mock-old'})
        assert stale['fresh'] is False and query()==''
        call('claim_agent_request',{'request_id':second})
        fresh=call('complete_agent_request',{'request_id':second,'output':'mock-new'})
        assert fresh['fresh'] and 'mock-new' in query() and 'mock-old' not in query()
        assert call('complete_agent_request',{'request_id':second,'output':'mock-new'})['duplicate']
        call('complete_agent_request',{'request_id':second,'output':'conflict'},error=True)
        call('apply_changes',{'changes':[{'op':'insert','predicate':'agent_response','values':[second,'bypass']}]},error=True)
        # Advancing input retracts the former current output inside Differential.
        third=submit('agent-a',3,'latest payload')
        assert query()==''
        fourth=submit('agent-a',4,'unclaimed replacement')
        call('claim_agent_request',{'request_id':third},error=True)
        other=submit('agent-b',1,'other payload')
        call('claim_agent_request',{'request_id':other})
        call('complete_agent_request',{'request_id':other,'output':'mock-other'})
        assert 'mock-other' in query()
        statuses=call('agent_request_status',{})
        assert statuses['durability']=='session-local'
        assert len(statuses['requests'])==5
        print('PASS: registered operation -> real DDlog; identity, duplicates, revisions, claims, stale completions, fresh results, retractions, conflicting results, isolated entities. External worker MOCK.')
    finally:
        Path(os.environ.get('AGENT_REQUEST_RECEIPTS','/tmp/registered-agent-receipts.json')).write_text(json.dumps(receipts,indent=2))
        p.stdin.close();p.wait(timeout=10)
