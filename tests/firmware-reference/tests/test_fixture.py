import contextlib, hashlib, importlib.util, io, json, os, pathlib, shutil, subprocess, sys, tempfile, unittest
from unittest import mock
BASE = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BASE))
(BASE / 'output').mkdir(exist_ok=True)
import inputs
TOOLS = {key: pathlib.Path('/explicit/' + key) for key in inputs.PATH_KEYS}
def module(name):
    spec = importlib.util.spec_from_file_location(name, BASE / (name + '.py'))
    m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); return m
prep, runner = module('prepare'), module('run')
def parse_cpio(data):
    out={}; off=0
    while True:
        assert data[off:off+6] == b'070701'
        fields=[int(data[off+6+i*8:off+14+i*8],16) for i in range(13)]
        size,namesize=fields[6],fields[11]; off+=110
        name=data[off:off+namesize-1].decode(); off+=namesize; off=(off+3)&~3
        payload=data[off:off+size]; off+=size; off=(off+3)&~3
        if name=='TRAILER!!!': break
        out[name]=(fields,payload)
    return out
class Fixture(unittest.TestCase):
    def test_both_cleanup_passes_fit_wall_bounds(self):
        self.assertEqual(runner.CLEANUP_WAIT_BOUND_SECONDS,2*(2+2)*2)
        self.assertLessEqual(runner.BOOT_WORK_SECONDS+runner.CLEANUP_WAIT_BOUND_SECONDS,180)
        self.assertLessEqual(runner.CAMPAIGN_WORK_SECONDS+runner.CLEANUP_WAIT_BOUND_SECONDS,600)
    def test_cpio_exact_pid1_contract(self):
        token='a1'*16; c=parse_cpio(prep.cpio(b'ELF-PID1',token))
        self.assertEqual(c['etc/vroom-reference-token'][1],token.encode())
        self.assertEqual(len(c['etc/vroom-reference-token'][1]),32)
        self.assertEqual(c['init'][0][1],0o100755)
        self.assertEqual(c['dev/console'][0][1],0o020600)
        self.assertEqual(c['dev/console'][0][9:11],[5,1])
        self.assertEqual(c['dev/null'][0][9:11],[1,3])
        self.assertFalse(any('..' in n or n.startswith('/') for n in c))
    def test_command_isolation(self):
        t,q=runner.commands(pathlib.Path('/tmp/owned'),pathlib.Path('/owned/accept.fat'),'seal',TOOLS)
        self.assertEqual(q[q.index('-smp')+1],'1');self.assertEqual(q[q.index('-m')+1],'1024')
        self.assertEqual(q[q.index('-nic')+1],'none')
        self.assertIn('if=none,id=esp,format=raw,readonly=on,file=/owned/accept.fat',q)
        for forbidden in ('-netdev','-virtfs','-fsdev','-incoming','-monitor stdio','tcp:'):
            self.assertFalse(any(forbidden in x for x in q+t))
        self.assertIn('type=unixio,path=/tmp/owned/tpm.sock,mode=0600,terminate',t)
    def test_efi_compile_and_sha(self):
        names = {'clang': 'VROOM_EFI_CLANG', 'lld_link': 'VROOM_EFI_LLD_LINK', 'llvm_library': 'VROOM_EFI_LLVM_LIBRARY'}
        if not all(os.environ.get(name) for name in names.values()):
            self.skipTest('set explicit VROOM_EFI_CLANG, VROOM_EFI_LLD_LINK and VROOM_EFI_LLVM_LIBRARY for compile check')
        tools = {key: inputs.file_path(os.environ[name], key != 'llvm_library') for key, name in names.items()}
        with tempfile.TemporaryDirectory(dir=BASE/'output') as td:
            d=pathlib.Path(td)
            header=''.join(prep.c_array(k,b'\x00') for k in ('PK_ESL','PK_AUTH','KEK_ESL','KEK_AUTH','DB_ESL','DB_AUTH','DB_UNAUTHORIZED','DB_UPDATE_1','DB_UPDATE_2'))
            header+=''.join('#define '+k+'_FINGERPRINT "'+('0'*64)+'"\n' for k in ('PK','KEK','DB','UNTRUSTED'))
            header+=prep.c_array('INITRD_SHA256',b'\0'*32)+'#define INITRD_HASH_HEX "'+('0'*64)+'"\n#define INITRD_SIZE 0ULL\n'
            header+=prep.c_array('UNSIGNED_SHA256',b'\0'*32)+'#define UNSIGNED_HASH_HEX "'+('0'*64)+'"\n#define UNSIGNED_SIZE 0ULL\n'
            (d/'fixture_public.h').write_text(header)
            prep.compile_efi(BASE/'src/reference.c',d/'reference.efi',d,tools)
            prep.compile_efi(BASE/'src/probe.c',d/'probe.efi',d,tools)
            # Native ordinary SHA test executable, not an EFI/guest execution.
            (d/'sha.c').write_text('#include <stdio.h>\n#include "efi_min.h"\n#include "sha256.h"\nint main(void){SHA256_CTX c;sha_init(&c);unsigned char b[777],o[32];size_t n;while((n=fread(b,1,sizeof(b),stdin)))sha_update(&c,b,n);sha_final(&c,o);for(int i=0;i<32;i++)printf("%02x",o[i]);return 0;}\n')
            subprocess.run([str(tools['clang']),'-I',str(BASE/'src'),str(d/'sha.c'),'-o',str(d/'sha')],check=True,capture_output=True)
            for data in (b'',b'abc',b'a'*55,b'b'*56,b'c'*64,bytes(range(256))*1000):
                p=subprocess.run([str(d/'sha')],input=data,capture_output=True,check=True,timeout=2)
                self.assertEqual(p.stdout.decode(),hashlib.sha256(data).hexdigest())
    def test_owned_process_cleanup(self):
        owner=runner.Owner()
        with open('/dev/null','wb') as f: p=owner.spawn([sys.executable,'-I','-c','import time; time.sleep(20)'],f)
        owner.stop_all();self.assertIsNotNone(p.poll())
    def test_failed_campaign_removes_state(self):
        with tempfile.TemporaryDirectory(dir=BASE/'output') as td:
            d=pathlib.Path(td);candidate=d/'candidate';candidate.mkdir();report=d/'report.json'
            vars_file=d/'vars';vars_file.write_bytes(b'blank-vars');tools={**TOOLS,'firmware_vars':vars_file}
            m={'scope':'local_qemu_firmware_tpm_reference_only','files':{'accept.fat':'unused'},'paths':tools}
            with mock.patch.object(inputs,'validate_paths',return_value=tools),mock.patch.object(runner,'verify',return_value=m),mock.patch.object(runner,'digest',return_value='unused'),mock.patch.object(runner,'one_boot',side_effect=runner.Failed('fixture failure')) as boot,contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run(candidate,'reviewed',report),1)
                boot.assert_called_once()
            result=json.loads(report.read_text());self.assertTrue(result['disposable_state_removed']);self.assertTrue(result['all_owned_processes_reaped']);self.assertEqual(result['boots'],[])
    def test_unreaped_child_preserves_private_state(self):
        with tempfile.TemporaryDirectory(dir=BASE/'output') as td:
            d=pathlib.Path(td);candidate=d/'candidate';candidate.mkdir();report=d/'report.json'
            vars_file=d/'vars';vars_file.write_bytes(b'blank-vars');tools={**TOOLS,'firmware_vars':vars_file}
            m={'scope':'local_qemu_firmware_tpm_reference_only','files':{'accept.fat':'unused'},'paths':tools}
            with mock.patch.object(inputs,'validate_paths',return_value=tools),mock.patch.object(runner,'verify',return_value=m),mock.patch.object(runner,'digest',return_value='unused'),mock.patch.object(runner,'one_boot',side_effect=runner.Failed('fixture failure')) as boot,mock.patch.object(runner.Owner,'stop_all',side_effect=runner.Unreaped('injected')),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run(candidate,'reviewed',report),1)
                boot.assert_called_once()
            result=json.loads(report.read_text());self.assertFalse(result['disposable_state_removed']);self.assertFalse(result['all_owned_processes_reaped'])
            retained=pathlib.Path(result['retained_state_path']);self.assertTrue(retained.is_dir())
            shutil.rmtree(retained)  # Mock only: there is no actual unreaped child.
    def test_failure_evidence_filters_secrets(self):
        with tempfile.TemporaryDirectory(dir=BASE/'output') as td:
            d=pathlib.Path(td)
            (d/'seal-serial.log').write_text('VROOM_TPM_REFERENCE '+json.dumps({'status':'fail','stage':'test_stage','secret':'NEVER_KEEP'})+'\n')
            output=runner.failure_evidence(d)
            self.assertNotIn('NEVER_KEEP',json.dumps(output));self.assertEqual(output[0]['diagnostic_records'][0]['stage'],'test_stage')
    def fixture(self,phase):
        h='1'*64; m={'certificate_sha256':{k:h for k in ('PK','KEK','DB','UNTRUSTED')},'initrd_sha256':h,'unsigned_probe_sha256':h,'token':'a'*32}
        e={'status':'pass','phase':'acceptance','secure_boot':1,'setup_mode':0,'pk_sha256':h,'kek_sha256':h,'db_sha256':h,
           'untrusted_signer_sha256':h,'initrd_sha256':h,'unsigned_probe_sha256':h,'authorized_update_step':1 if phase=='seal' else 2,
           'unsigned_refusal_status_hex':'800000000000000f','unsigned_image_handle_loaded':False}
        e.update({k:True for k in ('signed_child_executed','unsigned_child_refused','initrd_hash_verified','unauthorized_variable_update_rejected','authorized_variable_update_accepted','db_contents_unchanged','unsigned_image_not_started','unsigned_probe_hash_verified')})
        t={'schema':1,'token':'a'*32,'phase':phase,'status':'pass','pcr7_event_count':4}
        t.update({k:True for k in ('replay_matches','secure_boot_measured','password_auth_rejected','empty_policy_rejected','policy_unseal_verified')})
        t.update({k:h for k in ('pcr7_sha256','event_log_sha256','sealed_blob_sha256','secret_hash')})
        serial='VROOM_SIGNED_PROBE_EXECUTED\nVROOM_EFI_REFERENCE '+json.dumps(e)+'\nVROOM_TPM_REFERENCE '+json.dumps(t)+'\n'
        return m,{'phase':phase,'serial':serial}
    def test_acceptance_continuity(self):
        m,first=self.fixture('seal');runner.accept(first,m,None)
        _,second=self.fixture('unseal');runner.accept(second,m,first)
        self.assertTrue(second['tpm']['policy_unseal_verified'])
    def test_reject_missing_negative_proof(self):
        m,r=self.fixture('seal');r['serial']=r['serial'].replace('"password_auth_rejected": true','"password_auth_rejected": false')
        with self.assertRaises(runner.Failed):runner.accept(r,m,None)
    def test_reject_duplicate_probe(self):
        m,r=self.fixture('seal');r['serial']+='VROOM_SIGNED_PROBE_EXECUTED\n'
        with self.assertRaises(runner.Failed):runner.accept(r,m,None)
    def test_reject_changed_cold_pcr(self):
        m,first=self.fixture('seal');runner.accept(first,m,None)
        _,r=self.fixture('unseal');r['serial']=r['serial'].replace('"pcr7_sha256": "'+'1'*64+'"','"pcr7_sha256": "'+'2'*64+'"')
        with self.assertRaises(runner.Failed):runner.accept(r,m,first)
    def test_accept_documented_loaded_but_refused(self):
        m,r=self.fixture('seal');r['serial']=r['serial'].replace('800000000000000f','800000000000001a').replace('"unsigned_image_handle_loaded": false','"unsigned_image_handle_loaded": true')
        runner.accept(r,m,None)
    def test_reject_refusal_wrong_handle(self):
        for old,new in [('800000000000000f','800000000000001a'),('"unsigned_image_handle_loaded": false','"unsigned_image_handle_loaded": true')]:
            m,r=self.fixture('seal');r['serial']=r['serial'].replace(old,new)
            with self.assertRaises(runner.Failed):runner.accept(r,m,None)
    def test_reject_other_load_failures(self):
        for status in ('0000000000000000','8000000000000001','800000000000000e'):
            m,r=self.fixture('seal');r['serial']=r['serial'].replace('800000000000000f',status)
            with self.assertRaises(runner.Failed):runner.accept(r,m,None)
    def test_reject_unsigned_payload_drift(self):
        m,r=self.fixture('seal');m['unsigned_probe_sha256']='2'*64
        with self.assertRaises(runner.Failed):runner.accept(r,m,None)
if __name__=='__main__':unittest.main()
