import hashlib,importlib.util,json,os,shutil,sys,time,traceback
from pathlib import Path


def retryable(error):
 # Only transport/service failures retry; identity, integrity and local I/O do not.
 if isinstance(error, (ConnectionError, TimeoutError, EOFError)):
  return True
 if type(error).__name__ in {'IncompleteReadError','ReadTimeoutError','ConnectTimeoutError','ConnectionClosedError','EndpointConnectionError','ResponseStreamingError','HTTPClientError','ProtocolError'}:
  return True
 response=getattr(error,'response',{})
 return response.get('ResponseMetadata',{}).get('HTTPStatusCode') in {429,500,502,503,504}


def download(client,bucket,key,plan,root,status,disk,sleep=time.sleep,max_attempts=8):
 total=plan['archiveBytes'];expected=plan['archiveSha256']
 archive=root/'source.tar.zst';partial=root/'source.download'
 # The pinned whole-file digest is authoritative even when the object has no metadata.
 head=client.head_object(Bucket=bucket,Key=key)
 if head['ContentLength']!=total:raise RuntimeError('Pinned archive size changed')
 metadata=head.get('Metadata',{})
 if 'sha256' in metadata and metadata['sha256'].lower()!=expected:raise RuntimeError('Pinned archive SHA metadata changed')
 etag=head.get('ETag')
 if not etag:raise RuntimeError('Source ETag is required for conditional range requests')
 existing=archive if archive.exists() else partial
 count=0;digest=hashlib.sha256();reported=time.monotonic()
 if existing.exists():
  if existing.stat().st_size>total:raise RuntimeError('Existing archive exceeds pinned size')
  status('verify-existing-download',bytes=0,totalBytes=existing.stat().st_size)
  with existing.open('rb') as source:
   for block in iter(lambda:source.read(1024*1024),b''):
    digest.update(block);count+=len(block)
    if time.monotonic()-reported>=10:
     status('verify-existing-download',bytes=count,totalBytes=existing.stat().st_size);reported=time.monotonic()
  if archive.exists() and (count!=total or digest.hexdigest()!=expected):raise RuntimeError('Existing archive size or checksum mismatch')
 disk(total-count)
 if archive.exists():
  status('download-complete',bytes=count,totalBytes=total,reused=True)
  return archive
 for attempt in range(1,max_attempts+1):
  if count==total:break
  status('download',bytes=count,totalBytes=total,attempt=attempt,maxAttempts=max_attempts)
  body=None;failure=None
  try:
   try:
    response=client.get_object(Bucket=bucket,Key=key,Range=f'bytes={count}-',IfMatch=etag)
   except Exception as error:
    if not retryable(error):raise
    failure=error
   if failure is None:
    body=response['Body']
    if response.get('ETag')!=etag:raise RuntimeError('Download ETag differs from source identity')
    if response.get('ResponseMetadata',{}).get('HTTPStatusCode')!=206:raise RuntimeError('Server did not honor range request')
    if response.get('ContentRange')!=f'bytes {count}-{total-1}/{total}' or response.get('ContentLength')!=total-count:
     raise RuntimeError('Invalid download Content-Range or response length')
    # Append only after validating response identity and exact range.
    with partial.open('ab') as output:
     if output.tell()!=count:raise RuntimeError('Partial archive length changed unexpectedly')
     while count<total:
      try:block=body.read(min(1024*1024,total-count))
      except Exception as error:
       if not retryable(error):raise
       failure=error;break
      if not block:
       failure=EOFError('Download stream ended before pinned size');break
      if len(block)>total-count:raise RuntimeError('Archive larger than pinned size')
      disk(len(block));output.write(block);digest.update(block);count+=len(block)
      if time.monotonic()-reported>=10:
       status('download',bytes=count,totalBytes=total,attempt=attempt,maxAttempts=max_attempts);reported=time.monotonic()
     output.flush();os.fsync(output.fileno())
    if partial.stat().st_size!=count:raise RuntimeError('Partial archive length differs from verified bytes')
  finally:
   if body is not None:body.close()
  if failure is not None:
   # Never log SDK exception text, which could contain request details.
   if attempt==max_attempts:raise RuntimeError(f'Download retries exhausted at {count}/{total} bytes ({type(failure).__name__})')
   delay=min(30,2**(attempt-1));status('download-retry',bytes=count,totalBytes=total,attempt=attempt,errorType=type(failure).__name__,retryInSeconds=delay)
   sleep(delay)
 if count!=total or digest.hexdigest()!=expected:raise RuntimeError('Archive size or checksum mismatch')
 partial.rename(archive)
 status('download-complete',bytes=count,totalBytes=total,reused=False)
 return archive


def main():
 ROOT=Path(os.environ.get('PREFILL_JOB_ROOT','/run/job'));SNAPSHOT=Path(os.environ.get('PREFILL_SNAPSHOT_ROOT','/snapshot'))
 PLAN=json.loads((ROOT/'plan.json').read_text());STATUS=ROOT/'bootstrap-status.json';started=time.time()
 def status(stage,**values):
  data=dict(stage=stage,updatedAt=time.time(),elapsedSeconds=round(time.time()-started,2),**values)
  temp=STATUS.with_suffix('.tmp');temp.write_text(json.dumps(data,indent=2)+'\n');temp.replace(STATUS)
  print(json.dumps(data),flush=True)
 def disk(extra=0):
  if shutil.disk_usage(ROOT).free < 80*1024**3+extra:raise RuntimeError('80 GiB disk reserve would be exceeded')
 try:
  spec=importlib.util.spec_from_file_location('runtime',SNAPSHOT/'runtime.py');runtime=importlib.util.module_from_spec(spec);spec.loader.exec_module(runtime)
  loaded=[]
  for line in Path(os.environ.get('PREFILL_CREDENTIALS','/credentials/r2.env')).read_text().splitlines():
   if '=' in line and not line.startswith('#'):
    key,value=line.split('=',1);key=key.strip();os.environ[key]=value.strip().strip('"\'');loaded.append(key)
  bucket,key=runtime.s3_location(PLAN['sourceUri']);client=runtime.client()
  archive=download(client,bucket,key,PLAN,ROOT,status,disk)
  for name in loaded:os.environ.pop(name,None)
  del client
  status('extract',archiveVerified=True)
  original=runtime.BoundedReader
  class ReserveReader(original):
   def read(self,size=-1):
    disk();return super().read(size)
  runtime.BoundedReader=ReserveReader
  pending=ROOT/'extract.pending';pending.mkdir();runtime.extract(archive,pending,PLAN['maxExpandedGiB']*1024**3)
  seeds=list(pending.rglob('seed.json'))
  if len(seeds)!=1 or seeds[0].parent!=pending:raise RuntimeError('Expected one root-level seed bundle')
  status('verify-source');runtime.verify_bundle(pending)
  report=json.loads((pending/'report.json').read_text())
  if report.get('contracts')!=PLAN['contracts'] or report.get('entities')!=PLAN['entities']:raise RuntimeError('Full-source inventory differs from pinned plan')
  source=ROOT/'source';pending.rename(source)
  (ROOT/'source-report.json').write_text(json.dumps({'verified':True,'uri':PLAN['sourceUri'],'archiveSha256':PLAN['archiveSha256'],'contracts':PLAN['contracts'],'entities':PLAN['entities']},indent=2)+'\n')
  status('pipeline',sourceVerified=True)
  os.execv(sys.executable,[sys.executable,str(SNAPSHOT/'run-full-prefill.py'),'--bin',str(SNAPSHOT/'arkiv-proposer'),'--converter',str(SNAPSHOT/'sourcify-prefill.py'),'--source',str(source),'--base',str(SNAPSHOT/'genesis.bin'),'--key',str(SNAPSHOT/'fixture.key'),'--out',str(ROOT/'prefill'),'--sponsor','0x1a642f0e3c3af545e7acbd38b07251b3990914f1','--reserve-gib','80'])
 except BaseException as error:
  status('failed',error=f'{type(error).__name__}: {error}');traceback.print_exc();sys.exit(1)


if __name__=='__main__':
 main()
