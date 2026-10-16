#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
#define BAD 0xFFFFFFFFu
#define R 0x80000000u
#define W 0x40000000u
#define ROOT L"\\Flash Disk\\PocketHLE-VFS-PROBE"
#define FILE L"\\Flash Disk\\PocketHLE-VFS-PROBE\\save.bin"
#define OTHER L"\\Flash Disk\\PocketHLE-VFS-PROBE\\other.bin"
#define IPC L"\\Flash Disk\\PocketHLE-VFS-PROBE\\ipc.bin"
#define RAMROOT L"\\PocketHLE-VFS-RAM-PROBE"
#define RAMFILE RAMROOT L"\\data.bin"
#define ASSET L"\\SD Card\\GZVT999998\\asset.bin"
typedef struct {U32 process,thread,pid,tid;} Info;
typedef struct {U32 length,load,total,avail,pt,pa,vt,va;} Mem;
static U32 open_file(const W16 *p,U32 a,U32 sh,U32 d){return CreateFileW(p,a,sh,0,d,0,0);}
static U32 io_write(U32 h,const void *b,U32 n){U32 w=0;return WriteFile(h,b,n,&w,0)&&w==n;}
static U32 eq(const char*a,const char*b,U32 n){U32 i;for(i=0;i<n;i++)if(a[i]!=b[i])return 0;return 1;}
#ifdef WORKER
__attribute__((section(".text.entry"))) U32 entry(void){
 const W16 *mode=GetCommandLineW();U32 h,x,n=0,data[1];char b[4];
 if(mode[0]=='e'){SetLastError(123);h=open_file(FILE,R,3,3);return h==BAD&&GetLastError()==32?71:1;}
 if(mode[0]=='m'){h=open_file(RAMFILE,R,3,3);if(h==BAD)return 10;if(!ReadFile(h,b,3,&n,0)||n!=3||!eq(b,"RAM",3))return 11;CloseHandle(h);return 74;}
 if(mode[0]=='r'){SetLastError(123);h=open_file(FILE,W,3,3);if(h!=BAD||GetLastError()!=32)return 2;h=open_file(FILE,R,1,3);if(h==BAD)return 3;CloseHandle(h);return 72;}
 if(mode[0]=='u'){
  h=open_file(IPC,R,3,3);if(h==BAD)return 4;if(!ReadFile(h,data,4,&n,0)||n!=4)return 5;CloseHandle(h);
  h=data[0];if(!ReadFile(h,b,3,&n,0)||n!=3||!eq(b,"DEF",3))return 6;
  x=CreateEventW(0,1,0,L"PocketHLE.VFS.READY");SetEvent(x);CloseHandle(x);
  x=CreateEventW(0,1,0,L"PocketHLE.VFS.RELEASE");if(WaitForSingleObject(x,5000)!=0)return 7;CloseHandle(x);
  if(!CloseHandle(h))return 8;return 73;
 }
 return 9;
}
#else
static char report[16000];static U32 used,checks,failures;
static U32 append(char*d,const char*s){U32 n=0;while(s[n]){d[n]=s[n];n++;}return n;}
static void line(const char*s){used+=append(report+used,s);}
static void check(const char*s,U32 good){checks++;if(!good)failures++;line(good?"PASS ":"FAIL ");line(s);line("\r\n");}
static void hex(U32 n){char b[11];U32 i;b[0]='0';b[1]='x';for(i=0;i<8;i++)b[2+i]="0123456789abcdef"[(n>>(28-4*i))&15];b[10]=0;line(b);}
static U32 launch(Info*i,const W16*mode,U32 flags){return CreateProcessW(L"\\SD Card\\GZVT999998\\vfsworker.exe",mode,0,0,0,flags,0,0,0,i);}
static U32 done(Info*i,U32 expected){U32 code=0,ok=WaitForSingleObject(i->process,5000)==0&&GetExitCodeProcess(i->process,&code)&&code==expected;CloseHandle(i->thread);CloseHandle(i->process);return ok;}

#define PERSIST L"\\Flash Disk\\PocketHLE-VFSTEST-PERSIST-v3.bin"
static U32 persistence_run=0;
static U32 persistence_read(void) {
    U32 h,n=0,data[3]={0};
    h=open_file(PERSIST,R,3,3);
    if(h==BAD){if(GetLastError()==2){line("INFO persistence.first_run\r\n");return 1;}return 0;}
    if(!ReadFile(h,data,12,&n,0)||n!=12||GetFileSize(h,0)!=12||data[0]!=0x56544633||data[1]!=1||data[2]==0||data[2]==0xffffffff){CloseHandle(h);return 0;}
    CloseHandle(h);persistence_run=data[2];line("INFO persistence.prior_successful_runs=");hex(persistence_run);line("\r\n");return 1;
}
static void persistence_write(void) {
    U32 data[3]={0x56544633,1,persistence_run+1},h=open_file(PERSIST,W,3,persistence_run?3:1);
    U32 ok=h!=BAD&&io_write(h,data,12)&&FlushFileBuffers(h);
    if(h!=BAD&&!CloseHandle(h))ok=0;
    check("persistence.commit_next_run",ok);
}
static void repeated_files(void) {
    Mem before,after;U32 i,good=1,h,j,n,alias,free_before[2],free_after[2],total[2],dummy[2];char b[8];
    before.length=sizeof(before);GlobalMemoryStatus(&before);
    GetDiskFreeSpaceExW(L"\\Flash Disk\\",free_before,total,dummy);
    for(i=0;i<64;i++) {
        alias=0;h=open_file(FILE,R|W,0,1);
        if(h==BAD){good=0;break;}
        if(!io_write(h,"CYCLE",5)||!FlushFileBuffers(h)||!DuplicateHandle(GetCurrentProcess(),h,GetCurrentProcess(),&alias,0,0,2))good=0;
        if(!CloseHandle(h))good=0;
        SetLastError(123);if(ReadFile(h,b,1,&n,0)||GetLastError()!=6)good=0;
        SetLastError(123);j=open_file(FILE,R,3,3);if(j!=BAD||GetLastError()!=32)good=0;if(j!=BAD)CloseHandle(j);
        if(alias){if(SetFilePointer(alias,0,0,0)!=0||!ReadFile(alias,b,5,&n,0)||n!=5||!eq(b,"CYCLE",5))good=0;if(!CloseHandle(alias))good=0;}
        if(!DeleteFileW(FILE))good=0;
        if(!good)break;
    }
    check("cycles.64_duplicate_flush_close_delete",good&&i==64);
    after.length=sizeof(after);GlobalMemoryStatus(&after);check("cycles.exact_ram_refund",before.avail==after.avail);
    GetDiskFreeSpaceExW(L"\\Flash Disk\\",free_after,total,dummy);check("cycles.exact_flash_space_refund",free_before[0]==free_after[0]&&free_before[1]==free_after[1]);
}
__attribute__((section(".text.entry"))) U32 entry(void){
 U32 h,j,n=0,alias=0,ready,release,f,free0[2],free1[2],total[2],dummy[2],found[150],count=0;char b[16];Info info;Mem before,after;void *protected_out;
 {extern char __image_start[],__image_end[];const volatile unsigned char*p=(const volatile unsigned char*)__image_start;volatile unsigned char sink=0;while(p<(const volatile unsigned char*)__image_end){sink^=*p;p+=4096;}(void)sink;}
 line("PocketHLE native ARM VFS test v3\r\n");
 DeleteFileW(L"\\Flash Disk\\VFSTEST.TXT");
 if(GetFileAttributesW(ROOT)!=BAD){line("FAIL fixture.directory_already_exists; no existing files touched\r\n");failures++;goto finish;}
 check("persistence.read_previous_run",persistence_read());if(failures)goto finish;
 check("directories.create",CreateDirectoryW(ROOT,0));SetLastError(123);check("directories.existing_error",!CreateDirectoryW(ROOT,0)&&GetLastError()==183);
 SetLastError(123);check("directories.missing_parent",!CreateDirectoryW(ROOT L"\\absent\\child",0)&&GetLastError()==3);
 SetLastError(123);check("create.missing_existing",open_file(FILE,R,3,3)==BAD&&GetLastError()==2);
 h=open_file(FILE,R|W,3,1);check("create.new",h!=BAD);check("write.initial",io_write(h,"ABCDEF",6));CloseHandle(h);
 SetLastError(123);check("create.new_existing_refused",open_file(FILE,W,3,1)==BAD&&GetLastError()==80);
 SetLastError(123);h=open_file(FILE,W,3,4);check("create.open_always_preserves",h!=BAD&&GetLastError()==183&&GetFileSize(h,0)==6);CloseHandle(h);
 SetLastError(123);h=open_file(FILE,R|W,3,2);check("create.always_truncates_readwrite",h!=BAD&&GetLastError()==183&&GetFileSize(h,0)==0);check("write.after_truncate",io_write(h,"ABCDEF",6));CloseHandle(h);
 h=open_file(FILE,W,3,5);check("create.truncate_existing",h!=BAD&&GetFileSize(h,0)==0);CloseHandle(h);
 SetLastError(123);check("create.truncate_missing",open_file(OTHER,W,3,5)==BAD&&GetLastError()==2);
 SetLastError(123);check("create.invalid_disposition",open_file(FILE,R,3,0)==BAD&&GetLastError()==87);
 SetLastError(123);check("create.invalid_share",open_file(FILE,R,4,3)==BAD&&GetLastError()==87);
 h=open_file(FILE,R|W,3,3);check("write.restore",io_write(h,"ABCDEF",6));SetFilePointer(h,3,0,0);
 SetLastError(123);check("seek.negative_refused_and_preserved",SetFilePointer(h,BAD,0,0)==BAD&&GetLastError()==131&&SetFilePointer(h,0,0,1)==3);
 n=1;SetLastError(123);check("seek.high_nonzero_rejected",SetFilePointer(h,0,&n,0)==BAD&&GetLastError()==87&&SetFilePointer(h,0,0,1)==3);
 n=0;check("seek.high_zero_supported",SetFilePointer(h,0,&n,1)==3&&n==0);
 SetLastError(123);check("read.null_buffer_preserves_position",!ReadFile(h,0,1,&n,0)&&GetLastError()==87&&SetFilePointer(h,0,0,1)==3);
 SetLastError(123);check("read.bad_count_pointer_preserves_position",!ReadFile(h,b,1,(void*)0xDEAD0000,0)&&GetLastError()==87&&SetFilePointer(h,0,0,1)==3);
 protected_out=VirtualAlloc(0,4096,0x3000,2);
 SetLastError(123);check("read.readonly_output_preserves_position",protected_out&&!ReadFile(h,b,1,protected_out,0)&&GetLastError()==87&&SetFilePointer(h,0,0,1)==3);
 SetLastError(123);check("read.readonly_buffer_preserves_position",protected_out&&!ReadFile(h,protected_out,1,&n,0)&&GetLastError()==87&&SetFilePointer(h,0,0,1)==3);
 SetLastError(123);check("write.readonly_output_preserves_file",protected_out&&!WriteFile(h,"X",1,protected_out,0)&&GetLastError()==87&&GetFileSize(h,0)==6);
 if(protected_out)VirtualFree(protected_out,0,0x8000);
 check("read.exact_bytes",ReadFile(h,b,3,&n,0)&&n==3&&eq(b,"DEF",3));check("read.eof",ReadFile(h,b,3,&n,0)&&n==0);
 SetLastError(123);check("write.bad_count_pointer_preserves_file",!WriteFile(h,"X",1,(void*)0xDEAD0000,0)&&GetLastError()==87&&GetFileSize(h,0)==6);
 check("flush.success",FlushFileBuffers(h));CloseHandle(h);
 h=open_file(FILE,R,3,3);SetLastError(123);check("write.readonly_handle_denied",!WriteFile(h,"X",1,&n,0)&&GetLastError()==5);CloseHandle(h);
 h=open_file(FILE,W,3,3);SetLastError(123);check("read.writeonly_handle_denied",!ReadFile(h,b,1,&n,0)&&GetLastError()==5);CloseHandle(h);
 h=open_file(FILE,0,3,3);SetLastError(123);check("query.no_read_access",h!=BAD&&!ReadFile(h,b,1,&n,0)&&GetLastError()==5);CloseHandle(h);
 check("attributes.set_readonly_hidden",SetFileAttributesW(FILE,3)&&((GetFileAttributesW(FILE)&3)==3));
 j=FindFirstFileW(FILE,found);check("attributes.enumeration_matches",j!=BAD&&(found[0]&3)==3);FindClose(j);
 SetLastError(123);check("attributes.readonly_open_refused",open_file(FILE,W,3,3)==BAD&&GetLastError()==5);SetLastError(123);check("attributes.readonly_delete_refused",!DeleteFileW(FILE)&&GetLastError()==5);check("attributes.reset",SetFileAttributesW(FILE,0x80));
 h=open_file(FILE,R,0,3);SetLastError(123);check("sharing.local_exclusive",open_file(FILE,R,3,3)==BAD&&GetLastError()==32);
 check("sharing.child_exclusive_launch",launch(&info,L"e",0));check("sharing.child_exclusive_refused",done(&info,71));
 check("sharing.duplicate_local",DuplicateHandle(GetCurrentProcess(),h,GetCurrentProcess(),&alias,0,0,2));CloseHandle(h);
 SetLastError(123);check("sharing.duplicate_keeps_exclusive",open_file(FILE,R,3,3)==BAD&&GetLastError()==32);CloseHandle(alias);
 h=open_file(FILE,R,1,3);check("sharing.child_readonly_launch",launch(&info,L"r",0));check("sharing.child_read_allowed_write_refused",done(&info,72));CloseHandle(h);
 ready=CreateEventW(0,1,0,L"PocketHLE.VFS.READY");release=CreateEventW(0,1,0,L"PocketHLE.VFS.RELEASE");
 h=open_file(FILE,R,0,3);SetFilePointer(h,3,0,0);check("sharing.transfer_child_launch",launch(&info,L"u",4));check("sharing.transfer_duplicate",DuplicateHandle(GetCurrentProcess(),h,info.process,&alias,0,0,2));
 j=open_file(IPC,W,3,2);check("sharing.transfer_publish",io_write(j,&alias,4));CloseHandle(j);CloseHandle(h);ResumeThread(info.thread);
 check("sharing.transfer_position_shared",WaitForSingleObject(ready,5000)==0);SetLastError(123);check("sharing.child_alias_keeps_exclusive",open_file(FILE,R,3,3)==BAD&&GetLastError()==32);
 SetEvent(release);check("sharing.child_close_releases",done(&info,73));CloseHandle(ready);CloseHandle(release);DeleteFileW(IPC);
 h=open_file(FILE,R,3,3);check("sharing.reopen_after_last_close",h!=BAD);CloseHandle(h);
 f=fopen("\\Flash Disk\\PocketHLE-VFS-PROBE\\other.bin","r+");check("crt.rplus_missing_not_created",f==0&&GetFileAttributesW(OTHER)==BAD);
 f=fopen("\\Flash Disk\\PocketHLE-VFS-PROBE\\save.bin","w+");check("crt.wplus_truncates",f!=0&&GetFileSize(f,0)==0);check("crt.fwrite",fwrite("ONE",1,3,f)==3);fclose(f);
 f=fopen("\\Flash Disk\\PocketHLE-VFS-PROBE\\save.bin","a");check("crt.append_preserves",f!=0&&GetFileSize(f,0)==3);fseek(f,0,0);check("crt.append_after_seek",fwrite("TWO",1,3,f)==3);fclose(f);
 f=fopen("\\Flash Disk\\PocketHLE-VFS-PROBE\\save.bin","a+");fseek(f,0,0);check("crt.aplus_reads",fread(b,1,6,f)==6&&eq(b,"ONETWO",6));fseek(f,0,0);check("crt.aplus_appends_after_seek",fwrite("X",1,1,f)==1);fclose(f);
 f=fopen("\\Flash Disk\\PocketHLE-VFS-PROBE\\save.bin","r+");check("crt.rplus_preserves",f!=0&&GetFileSize(f,0)==7);fclose(f);
 SetLastError(123);check("rename.open_source_refused",(h=open_file(FILE,R,3,3))!=BAD&&!MoveFileW(FILE,OTHER)&&GetLastError()==32);CloseHandle(h);
 h=open_file(OTHER,W,3,1);io_write(h,"DEST",4);CloseHandle(h);SetLastError(123);check("rename.existing_destination_refused",!MoveFileW(FILE,OTHER)&&GetLastError()==183);
 h=open_file(OTHER,R,3,3);check("rename.destination_preserved",ReadFile(h,b,4,&n,0)&&eq(b,"DEST",4));CloseHandle(h);DeleteFileW(OTHER);
 check("rename.success",MoveFileW(FILE,OTHER)&&GetFileAttributesW(FILE)==BAD&&GetFileAttributesW(OTHER)!=BAD);
 SetLastError(123);check("directories.nonempty_refused",!RemoveDirectoryW(ROOT)&&GetLastError()==145);
 h=FindFirstFileW(ROOT L"\\*.bin",found);check("find.first",h!=BAD&&found[8]==7);while(FindNextFileW(h,found))count++;
 check("find.end_error",count==0&&GetLastError()==18);check("find.close",FindClose(h));SetLastError(123);check("find.double_close",!FindClose(h)&&GetLastError()==6);
 SetLastError(123);check("find.missing_error",FindFirstFileW(ROOT L"\\absent*",found)==BAD&&GetLastError()==2);
 SetLastError(123);check("attributes.missing_error",GetFileAttributesW(FILE)==BAD&&GetLastError()==2);
 SetLastError(123);check("delete.missing_error",!DeleteFileW(FILE)&&GetLastError()==2);check("delete.success",DeleteFileW(OTHER));
 check("directories.remove_real",RemoveDirectoryW(ROOT)&&GetFileAttributesW(ROOT)==BAD);check("directories.recreate",CreateDirectoryW(ROOT,0));
 h=open_file(ASSET,R,3,3);check("paths.case_insensitive",h!=BAD);CloseHandle(h);
 h=open_file(L"\\sd card\\gzvt999998\\ASSET.BIN",R,3,3);check("paths.case_insensitive_again",h!=BAD);CloseHandle(h);
 SetLastError(123);check("paths.readonly_card_cannot_create_read_handle",open_file(L"\\SD Card\\GZVT999998\\new.bin",R,3,4)==BAD&&GetLastError()==5);
 j=FindFirstFileW(ASSET,found);check("attributes.card_readonly",j!=BAD&&(found[0]&1)==1);FindClose(j);
 SetLastError(123);check("paths.readonly_card_write_refused",open_file(ASSET,W,3,3)==BAD&&GetLastError()==5);
 SetLastError(123);check("paths.escape_refused",open_file(ROOT L"\\..\\..\\..\\escape.bin",W,3,2)==BAD&&GetLastError()==5);
 check("ram.create_directory",CreateDirectoryW(RAMROOT,0));
 h=open_file(RAMFILE,R|W,3,1);check("ram.create_and_write",h!=BAD&&io_write(h,"RAM",3));CloseHandle(h);
 SetLastError(123);check("ram.create_new_existing",open_file(RAMFILE,W,3,1)==BAD&&GetLastError()==80);
 h=open_file(RAMFILE,W,3,4);check("ram.open_always_preserves",h!=BAD&&GetFileSize(h,0)==3);CloseHandle(h);
 check("ram.child_reads_shared_store_launch",launch(&info,L"m",0));check("ram.child_reads_shared_store",done(&info,74));
 h=open_file(RAMFILE,R|W,0,3);SetLastError(123);check("ram.exclusive_open",open_file(RAMFILE,R,3,3)==BAD&&GetLastError()==32);
 SetLastError(123);check("ram.open_delete_refused",!DeleteFileW(RAMFILE)&&GetLastError()==32);CloseHandle(h);
 h=open_file(RAMFILE,R|W,3,2);check("ram.create_always_truncates",h!=BAD&&GetFileSize(h,0)==0);io_write(h,"DATA",4);SetFilePointer(h,2,0,0);check("ram.set_end_of_file",SetEndOfFile(h)&&GetFileSize(h,0)==2);CloseHandle(h);
 check("ram.rename_file",MoveFileW(RAMFILE,RAMROOT L"\\new.bin"));check("ram.delete_file",DeleteFileW(RAMROOT L"\\new.bin"));check("ram.remove_directory",RemoveDirectoryW(RAMROOT));
 check("volumes.object_store",GetDiskFreeSpaceExW(0,free1,total,dummy)&&total[0]>0&&free1[0]<=total[0]);
 check("volumes.flash_32mib",GetDiskFreeSpaceExW(L"\\Flash Disk\\",free0,total,dummy)&&total[0]==32*1024*1024&&total[1]==0&&free0[1]==0);
 SetLastError(123);check("volumes.missing_directory_error",!GetDiskFreeSpaceExW(ROOT L"\\absent",free1,total,dummy)&&GetLastError()==3);
 check("volumes.sd_separate",GetDiskFreeSpaceExW(L"\\SD Card\\GZVT999998\\",free1,total,dummy)&&total[0]>=64*1024*1024&&free1[0]<total[0]);
 GetDiskFreeSpaceExW(L"\\Flash Disk\\",free0,total,dummy);before.length=sizeof(before);after.length=sizeof(after);GlobalMemoryStatus(&before);GlobalMemoryStatus(&after);GlobalMemoryStatus(&before);
 h=open_file(FILE,R|W,3,1);SetFilePointer(h,free0[0]+1,0,0);SetLastError(123);check("quota.overflow_refused",!SetEndOfFile(h)&&GetLastError()==112&&GetFileSize(h,0)==0);
 SetFilePointer(h,free0[0],0,0);check("quota.fill_available",SetEndOfFile(h));check("quota.full_reports_zero",GetDiskFreeSpaceExW(L"\\Flash Disk\\",free1,total,dummy)&&free1[0]==0);
 SetLastError(123);check("quota.write_refused_no_growth",!WriteFile(h,"X",1,&n,0)&&GetLastError()==112&&GetFileSize(h,0)==free0[0]);
 after.length=sizeof(after);GlobalMemoryStatus(&after);line("INFO quota.ram_before=");hex(before.avail);line(" ram_after=");hex(after.avail);line(" total_before=");hex(before.total);line(" total_after=");hex(after.total);line("\r\n");check("quota.nand_not_ram",before.avail==after.avail);
 SetFilePointer(h,0,0,0);check("quota.truncate_refunds",SetEndOfFile(h)&&GetDiskFreeSpaceExW(L"\\Flash Disk\\",free1,total,dummy)&&free1[0]==free0[0]);CloseHandle(h);
 check("cleanup.file",DeleteFileW(FILE));repeated_files();check("cleanup.directory",RemoveDirectoryW(ROOT));
 if(!failures)persistence_write();
finish:
 line(failures?"VFSTEST_RESULT FAIL checks=":"VFSTEST_RESULT PASS checks=");hex(checks);line(" failures=");hex(failures);line("\r\n");
 h=open_file(L"\\Flash Disk\\VFSTEST.TXT",W,3,2);if(h!=BAD){io_write(h,report,used);CloseHandle(h);}
 MessageBoxW(0,failures?L"FAIL: see VFSTEST.TXT":L"PASS: see VFSTEST.TXT",L"PocketHLE VFS test",0);return failures?1:0;
}
#endif
