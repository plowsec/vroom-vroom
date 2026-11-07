#include "efi_min.h"
#include "sha256.h"
/* Generated public certificates, authenticated enrollment signatures and initrd
 * digest only. No private key or unseal value is embedded in this EFI app. */
#include "fixture_public.h"
static EFI_SYSTEM_TABLE *S;
static EFI_HANDLE self;
static EFI_LOADED_IMAGE *loaded;
static U8 varbuf[16384],readbuf[65536],pathbuf[8192];
static EFI_GUID STEP_GUID={0xa8b22a31,0x26ae,0x4ef7,{0x98,0x68,0x65,0xe3,0xc5,0x5a,0xc9,0x91}};
static CHAR16 options[]=L"console=ttyS0,115200 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init panic=-1 loglevel=5 initrd=\\EFI\\BOOT\\INITRD.CPIO";
static void shutdown(void){S->RuntimeServices->ResetSystem(2,0,0,0);for(;;){}}
static void fail(const char*stage,EFI_STATUS rc){
 char hex[17];const char*d="0123456789abcdef";for(U32 i=0;i<16;i++)hex[i]=d[(rc>>(60-4*i))&15];hex[16]=0;
 print_ascii(S,"VROOM_EFI_REFERENCE {\"status\":\"fail\",\"stage\":\"");print_ascii(S,stage);print_ascii(S,"\",\"efi_status_hex\":\"");print_ascii(S,hex);print_ascii(S,"\"}\r\n");shutdown();
}
static void check(EFI_STATUS rc,const char*stage){if(rc!=EFI_SUCCESS)fail(stage,rc);}
static U8 flag(CHAR16*name){UINTN n=1;U8 value=255;U32 attrs=0;check(S->RuntimeServices->GetVariable(name,&GLOBAL_GUID,&attrs,&n,&value),"get_flag");if(n!=1||value>1)fail("bad_flag",EFIERR(2));return value;}
static void verify_var(CHAR16*name,EFI_GUID*guid,const U8*expected,UINTN n){
 UINTN size=sizeof(varbuf);U32 attrs=0;check(S->RuntimeServices->GetVariable(name,guid,&attrs,&size,varbuf),"get_enrolled_variable");
 if(size!=n||attrs!=EFI_VARIABLE_ATTRS||!equal_bytes(varbuf,expected,n))fail("variable_content_or_attributes",EFIERR(26));
}
static void verify_keys(void){verify_var(L"PK",&GLOBAL_GUID,PK_ESL,sizeof(PK_ESL));verify_var(L"KEK",&GLOBAL_GUID,KEK_ESL,sizeof(KEK_ESL));verify_var(L"db",&DB_GUID,DB_ESL,sizeof(DB_ESL));}
static void public_keys(void){print_ascii(S,"\"pk_sha256\":\"" PK_FINGERPRINT "\",\"kek_sha256\":\"" KEK_FINGERPRINT "\",\"db_sha256\":\"" DB_FINGERPRINT "\"");}
static U8 variable_enforcement(void){
 U8 step=0;UINTN n=1;U32 attrs=0;
 EFI_STATUS rc=S->RuntimeServices->GetVariable(L"VroomReferenceUpdateStep",&STEP_GUID,&attrs,&n,&step);
 if(rc==EFI_NOT_FOUND){step=0;}else{check(rc,"read_update_step");if(n!=1||attrs!=7||step!=1)fail("bad_update_step",EFIERR(2));}
 rc=S->RuntimeServices->SetVariable(L"db",&DB_GUID,EFI_VARIABLE_ATTRS,sizeof(DB_UNAUTHORIZED),(void*)DB_UNAUTHORIZED);
 if(rc!=EFI_SECURITY_VIOLATION)fail("unauthorized_update_was_not_rejected",rc);
 verify_var(L"db",&DB_GUID,DB_ESL,sizeof(DB_ESL));
 if(flag(L"SecureBoot")!=1||flag(L"SetupMode")!=0)fail("flags_after_rejected_update",EFIERR(26));
 if(step==0)rc=S->RuntimeServices->SetVariable(L"db",&DB_GUID,EFI_VARIABLE_ATTRS,sizeof(DB_UPDATE_1),(void*)DB_UPDATE_1);
 else rc=S->RuntimeServices->SetVariable(L"db",&DB_GUID,EFI_VARIABLE_ATTRS,sizeof(DB_UPDATE_2),(void*)DB_UPDATE_2);
 check(rc,"authorized_update");verify_var(L"db",&DB_GUID,DB_ESL,sizeof(DB_ESL));
 if(flag(L"SecureBoot")!=1||flag(L"SetupMode")!=0)fail("flags_after_authorized_update",EFIERR(26));
 step++;check(S->RuntimeServices->SetVariable(L"VroomReferenceUpdateStep",&STEP_GUID,7,1,&step),"write_update_step");
 U8 stored=0;n=1;attrs=0;check(S->RuntimeServices->GetVariable(L"VroomReferenceUpdateStep",&STEP_GUID,&attrs,&n,&stored),"reread_update_step");
 if(n!=1||attrs!=7||stored!=step)fail("update_step_persistence",EFIERR(2));return step;
}
static void*file_path(CHAR16*name){
 U8*base=0;check(S->BootServices->HandleProtocol(loaded->DeviceHandle,&DEVICE_PATH_GUID,(void**)&base),"get_device_path");
 UINTN n=0;for(;;){if(n+4>4096)fail("device_path_bound",EFIERR(2));U32 len=base[n+2]|((U32)base[n+3]<<8);if(len<4||n+len>4096)fail("device_path_format",EFIERR(2));if(base[n]==0x7f){if(base[n+1]!=0xff||len!=4)fail("device_path_end",EFIERR(2));break;}n+=len;}
 UINTN chars=0;while(name[chars]){if(++chars>200)fail("file_path_bound",EFIERR(2));}
 UINTN fn=4+2*(chars+1);copy_bytes(pathbuf,base,n);pathbuf[n]=4;pathbuf[n+1]=4;pathbuf[n+2]=(U8)fn;pathbuf[n+3]=(U8)(fn>>8);copy_bytes(pathbuf+n+4,name,2*(chars+1));n+=fn;pathbuf[n]=0x7f;pathbuf[n+1]=0xff;pathbuf[n+2]=4;pathbuf[n+3]=0;return pathbuf;
}
static void verify_file(CHAR16*name,const U8*expected,UINTN expected_size){
 EFI_SIMPLE_FILE_SYSTEM*fs=0;EFI_FILE*root=0,*f=0;
 check(S->BootServices->HandleProtocol(loaded->DeviceHandle,&FILE_SYSTEM_GUID,(void**)&fs),"filesystem");check(fs->OpenVolume(fs,&root),"open_volume");check(root->Open(root,&f,name,1,0),"open_bound_file");
 SHA256_CTX c;sha_init(&c);UINTN total=0;
 for(;;){UINTN n=sizeof(readbuf);check(f->Read(f,&n,readbuf),"read_bound_file");if(n>sizeof(readbuf)||total+n>16*1024*1024)fail("file_size_bound",EFIERR(2));if(n==0)break;sha_update(&c,readbuf,n);total+=n;}
 check(f->Close(f),"close_bound_file");check(root->Close(root),"close_volume");U8 digest[32];sha_final(&c,digest);
 if(total!=expected_size||!equal_bytes(digest,expected,32))fail("bound_file_integrity",EFIERR(26));
}
EFI_STATUS efi_main(EFI_HANDLE image,EFI_SYSTEM_TABLE*st){
 S=st;self=image;if(!st||!st->ConOut||!st->BootServices||!st->RuntimeServices)return EFIERR(2);
 check(S->BootServices->HandleProtocol(image,&LOADED_IMAGE_GUID,(void**)&loaded),"loaded_image");
 U8 setup=flag(L"SetupMode"),secure=flag(L"SecureBoot");
 if(setup==1){
  if(secure!=0)fail("initial_flags",EFIERR(26));
  check(S->RuntimeServices->SetVariable(L"db",&DB_GUID,EFI_VARIABLE_ATTRS,sizeof(DB_AUTH),(void*)DB_AUTH),"enroll_db");
  check(S->RuntimeServices->SetVariable(L"KEK",&GLOBAL_GUID,EFI_VARIABLE_ATTRS,sizeof(KEK_AUTH),(void*)KEK_AUTH),"enroll_kek");
  check(S->RuntimeServices->SetVariable(L"PK",&GLOBAL_GUID,EFI_VARIABLE_ATTRS,sizeof(PK_AUTH),(void*)PK_AUTH),"enroll_pk");
  verify_keys();if(flag(L"SetupMode")!=0)fail("enrollment_setup_mode",EFIERR(26));
  print_ascii(S,"VROOM_EFI_REFERENCE {\"status\":\"pass\",\"phase\":\"enroll\",\"initial_setup_mode\":1,\"initial_secure_boot\":0,\"setup_mode_after\":0,");public_keys();print_ascii(S,"}\r\n");shutdown();
 }
 if(secure!=1||setup!=0)fail("acceptance_flags",EFIERR(26));verify_keys();U8 update_step=variable_enforcement();
 verify_file(L"\\EFI\\BOOT\\INITRD.CPIO",INITRD_SHA256,INITRD_SIZE);
 verify_file(L"\\EFI\\BOOT\\UNSIGNED.EFI",UNSIGNED_SHA256,UNSIGNED_SIZE);
 EFI_HANDLE child=0;check(S->BootServices->LoadImage(0,self,file_path(L"\\EFI\\BOOT\\SIGNED.EFI"),0,0,&child),"signed_load");
 /* Returning EFI applications are unloaded by StartImage/Exit itself. */
 check(S->BootServices->StartImage(child,0,0),"signed_start");
 child=0;EFI_STATUS denied=S->BootServices->LoadImage(0,self,file_path(L"\\EFI\\BOOT\\UNSIGNED.EFI"),0,0,&child);
 /* UEFI LoadImage distinguishes reject-before-load from defer-after-load. */
 if(denied==EFI_ACCESS_DENIED){if(child)fail("access_denied_returned_handle",EFIERR(2));}
 else if(denied==EFI_SECURITY_VIOLATION){
  if(!child)fail("security_violation_missing_handle",EFIERR(2));
  EFI_LOADED_IMAGE*unsigned_info=0;
  check(S->BootServices->HandleProtocol(child,&LOADED_IMAGE_GUID,(void**)&unsigned_info),"unsigned_loaded_protocol");
  if(!unsigned_info)fail("unsigned_protocol_null",EFIERR(2));
  check(S->BootServices->UnloadImage(child),"unsigned_unload");
 }else fail("unsigned_not_policy_refused",denied);
 print_ascii(S,"VROOM_EFI_REFERENCE {\"status\":\"pass\",\"phase\":\"acceptance\",\"secure_boot\":1,\"setup_mode\":0,\"signed_child_executed\":true,\"unsigned_child_refused\":true,\"unauthorized_variable_update_rejected\":true,\"authorized_variable_update_accepted\":true,\"db_contents_unchanged\":true,\"authorized_update_step\":");print_ascii(S,update_step==1?"1":"2");
 print_ascii(S,",\"unsigned_refusal_status_hex\":\"");print_ascii(S,denied==EFI_ACCESS_DENIED?"800000000000000f":"800000000000001a");
 print_ascii(S,"\",\"unsigned_image_handle_loaded\":");print_ascii(S,denied==EFI_ACCESS_DENIED?"false":"true");
 print_ascii(S,",\"unsigned_image_not_started\":true,\"unsigned_probe_hash_verified\":true,\"unsigned_probe_sha256\":\"" UNSIGNED_HASH_HEX "\",\"untrusted_signer_sha256\":\"" UNTRUSTED_FINGERPRINT "\",\"initrd_hash_verified\":true,\"initrd_sha256\":\"" INITRD_HASH_HEX "\",");public_keys();print_ascii(S,"}\r\n");
 EFI_HANDLE kernel=0;check(S->BootServices->LoadImage(0,self,file_path(L"\\EFI\\BOOT\\LINUX.EFI"),0,0,&kernel),"signed_kernel_load");
 EFI_LOADED_IMAGE*ki=0;check(S->BootServices->HandleProtocol(kernel,&LOADED_IMAGE_GUID,(void**)&ki),"kernel_loaded_image");ki->LoadOptions=options;ki->LoadOptionsSize=sizeof(options);
 EFI_STATUS rc=S->BootServices->StartImage(kernel,0,0);fail("kernel_returned",rc);return EFIERR(1);
}
