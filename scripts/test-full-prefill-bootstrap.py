import hashlib, importlib.util, io, tempfile, unittest
from pathlib import Path
spec=importlib.util.spec_from_file_location('bootstrap',Path(__file__).parent/'full-prefill-bootstrap.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
DATA=b'0123456789abcdef'*200000
class Interrupted(io.BytesIO):
 def __init__(self,data):super().__init__(data);self.reads=0
 def read(self,size=-1):
  self.reads+=1
  if self.reads==2:raise ConnectionError('simulated')
  return super().read(min(size,123457))
class Client:
 def __init__(self):self.calls=[];self.mode=None
 def head_object(self,**kw):return {'ContentLength':len(DATA),'ETag':'"fixed"','Metadata':{'sha256':hashlib.sha256(DATA).hexdigest()}}
 def get_object(self,**kw):
  self.calls.append(kw);offset=int(kw['Range'][6:-1]);chunk=DATA[offset:]
  response={'ETag':'"fixed"','ContentLength':len(chunk),'ContentRange':f'bytes {offset}-{len(DATA)-1}/{len(DATA)}','ResponseMetadata':{'HTTPStatusCode':206},'Body':io.BytesIO(chunk)}
  if self.mode=='interrupt' and len(self.calls)==1:response['Body']=Interrupted(chunk)
  if self.mode=='eof' and len(self.calls)==1:response['Body']=io.BytesIO(chunk[:123])
  if self.mode=='status':response['ResponseMetadata']['HTTPStatusCode']=200
  if self.mode=='range':response['ContentRange']='bytes 0-1/2'
  if self.mode=='length':response['ContentLength']-=1
  if self.mode=='etag':response['ETag']='"changed"'
  if self.mode=='corrupt':response['Body']=io.BytesIO(b'X'+chunk[1:])
  if self.mode=='offline':raise ConnectionError('simulated')
  return response
class Tests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name);self.client=Client();self.events=[];self.sleeps=[]
  self.plan={'archiveBytes':len(DATA),'archiveSha256':hashlib.sha256(DATA).hexdigest()}
 def tearDown(self):self.tmp.cleanup()
 def run_download(self):return m.download(self.client,'b','k',self.plan,self.root,lambda stage,**kw:self.events.append((stage,kw)),lambda extra=0:None,self.sleeps.append,max_attempts=3)
 def test_resume_interrupted(self):
  (self.root/'source.download').write_bytes(DATA[:97]);self.client.mode='interrupt'
  result=self.run_download();self.assertEqual(result.read_bytes(),DATA)
  self.assertEqual([x['Range'] for x in self.client.calls],['bytes=97-','bytes=123554-']);self.assertTrue(all(x['IfMatch']=='"fixed"' for x in self.client.calls))
 def test_early_eof(self):
  self.client.mode='eof';self.assertEqual(self.run_download().read_bytes(),DATA);self.assertEqual(self.client.calls[1]['Range'],'bytes=123-')
 def test_reuse_valid_archive(self):
  (self.root/'source.tar.zst').write_bytes(DATA);self.run_download();self.assertEqual(self.client.calls,[])
 def test_complete_partial(self):
  (self.root/'source.download').write_bytes(DATA);self.run_download();self.assertEqual(self.client.calls,[]);self.assertFalse((self.root/'source.download').exists())
 def test_invalid_responses(self):
  for mode in ('status','range','length','etag'):
   with self.subTest(mode=mode):
    self.client.mode=mode;self.client.calls=[]
    with self.assertRaises(RuntimeError):self.run_download()
    self.assertEqual(len(self.client.calls),1);self.assertFalse((self.root/'source.download').exists())
 def test_digest_failure_not_retried(self):
  self.client.mode='corrupt'
  with self.assertRaisesRegex(RuntimeError,'checksum'):self.run_download()
  self.assertEqual(len(self.client.calls),1);self.assertFalse((self.root/'source.tar.zst').exists())
 def test_bad_existing_full(self):
  (self.root/'source.tar.zst').write_bytes(b'bad')
  with self.assertRaisesRegex(RuntimeError,'checksum'):self.run_download()
  self.assertEqual(self.client.calls,[])
 def test_bound_retries(self):
  self.client.mode='offline'
  with self.assertRaisesRegex(RuntimeError,'exhausted'):self.run_download()
  self.assertEqual(len(self.client.calls),3);self.assertEqual(self.sleeps,[1,2])
 def test_head_pin_failures(self):
  for changes in ({'ContentLength':1},{'ETag':None},{'Metadata':{'sha256':'bad'}}):
   original=Client().head_object();original.update(changes);self.client.head_object=lambda **kw:original
   with self.assertRaises(RuntimeError):self.run_download()
   self.assertEqual(self.client.calls,[])
if __name__=='__main__':unittest.main()
