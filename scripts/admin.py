#!/usr/bin/env python3
"""Call the private REST API without placing the admin key in argv or stdout."""
import argparse
import http.client
import json
import os
import pathlib
import re
import subprocess
import sys

ROOT=pathlib.Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--session',default=os.environ.get('VERTEX_SHIM_SESSION','dev'))
    parser.add_argument('--body',help='JSON request body; use - to read stdin')
    parser.add_argument('--key-file',type=pathlib.Path,help='New mode-0600 file for a one-time issued/rotated API key')
    parser.add_argument('method',choices=['GET','POST','PUT','DELETE'])
    parser.add_argument('path',help='Path beneath /api/private/v1, e.g. /projects')
    args=parser.parse_args()
    if not re.fullmatch('[a-z][a-z0-9-]{0,39}',args.session): parser.error('invalid session name')
    if not args.path.startswith('/') or args.path.startswith('//') or '\r' in args.path or '\n' in args.path: parser.error('invalid private API path')
    issue=args.method=='POST' and (args.path.endswith('/credentials') or args.path.endswith('/rotate'))
    if issue and args.key_file is None: parser.error('credential issuance requires --key-file to save the one-time secret')
    if args.key_file and args.key_file.exists(): parser.error('--key-file must name a new file')
    payload=None
    if args.body is not None:
        payload=args.body if args.body!='-' else sys.stdin.read()
        json.loads(payload)
    admin=(ROOT/'.state'/args.session/'admin-api-key').read_text().strip()
    address=subprocess.check_output(['podman','port',f'vertex-shim-{args.session}-proxy','8081/tcp'],text=True).strip()
    connection=http.client.HTTPConnection('127.0.0.1',int(address.rsplit(':',1)[1]),timeout=15)
    try:
        connection.request(args.method,'/api/private/v1'+args.path,body=payload,headers={'Authorization':'Bearer '+admin,'Content-Type':'application/json'})
        response=connection.getresponse();wire=response.read()
        result=json.loads(wire) if wire else None
    finally: connection.close()
    if not 200<=response.status<300:
        print(json.dumps({'http_status':response.status,'response':result}),file=sys.stderr);return 1
    if isinstance(result,dict) and 'api_key' in result:
        # This branch only follows requests for which a destination was required.
        access=result.pop('api_key')
        if not args.key_file: raise ValueError('unexpected secret response')
        args.key_file.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
        fd=os.open(args.key_file,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as stream:stream.write(access)
        result['key_file']=str(args.key_file)
    if result is not None: print(json.dumps(result,indent=2))
    else: print(json.dumps({'http_status':response.status}))
    return 0

if __name__=='__main__':
    try: sys.exit(main())
    except (OSError,ValueError,subprocess.SubprocessError,http.client.HTTPException):
        print('Admin request failed; check the session, JSON body, and destination file.',file=sys.stderr);sys.exit(1)
