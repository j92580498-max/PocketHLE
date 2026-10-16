#include "imports.h"
typedef unsigned int U32;
typedef unsigned short W16;
typedef struct {U32 process,thread,pid,tid;} Info;
typedef struct {U32 length,load,total,avail,pt,pa,vt,va;} MemStatus;
static U32 report,checks,failures,ready,tick,slot;
static U32 text(char *d,const char *s){U32 n=0;while(s[n]){d[n]=s[n];n++;}return n;}
static void line(const char *s){U32 n=0;while(s[n])n++;{U32 w=0;if(!WriteFile(report,s,n,&w,0)||w!=n)failures++;}}
static void check(const char *name,U32 good){char b[180];U32 n=0;checks++;if(!good)failures++;n+=text(b,good?"PASS ":"FAIL ");n+=text(b+n,name);b[n++]='\r';b[n++]='\n';b[n]=0;line(b);}
static U32 launch(Info *i,const W16 *mode,U32 flags){ResetEvent(ready);ResetEvent(tick);return CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",mode,0,0,0,flags,0,0,0,i);}
static void close_info(Info *i){CloseHandle(i->thread);CloseHandle(i->process);}
static U32 write_data(U32 *data){U32 n=0,h=CreateFileW(L"\\Flash Disk\\PROCDATA.BIN",0x40000000,3,0,2,0,0);if(h==0xFFFFFFFF)return 0;WriteFile(h,data,24,&n,0);CloseHandle(h);return n==24;}
static U32 parent_worker(void *arg){(void)arg;return 88;}
static U32 waited_code(Info *i,U32 expected){U32 code=0;return WaitForSingleObject(i->process,5000)==0&&GetExitCodeProcess(i->process,&code)&&code==expected;}
__attribute__((section(".text.entry"))) U32 entry(void){
    Info i={0},j={0};U32 data[6]={0},code=0,alias=0,h=0,id=0,file,n;
    {extern char __image_start[],__image_end[];volatile unsigned char sink=0;const volatile unsigned char *p=(const volatile unsigned char*)__image_start;while(p<(const volatile unsigned char*)__image_end){sink^=*p;p+=4096;}(void)sink;}
    report=CreateFileW(L"\\Flash Disk\\PROCTEST.TXT",0x40000000,3,0,2,0,0);if(report==0xFFFFFFFF)return 1;
    ready=CreateEventW(0,1,0,L"PocketHLE.PROC.READY");tick=CreateEventW(0,0,0,L"PocketHLE.PROC.TICK");
    slot=TlsAlloc();TlsSetValue(slot,0xAABBCCDD);
    SetLastError(123);check("process.no_automatic_handle_inheritance",CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",L"d",0,0,1,0,0,0,0,&i)==0&&GetLastError()==87);
    SetLastError(123);check("process.missing_image_error",CreateProcessW(L"\\SD Card\\absent-proc.exe",0,0,0,0,0,0,0,0,&i)==0&&GetLastError()==2);
    SetLastError(123);check("process.invalid_image_error",CreateProcessW(L"\\SD Card\\GZRT999999\\badproc.exe",0,0,0,0,0,0,0,0,&i)==0&&GetLastError()==193);
    SetLastError(123);check("process.unsupported_flags_error",CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",0,0,0,0,1,0,0,0,&i)==0&&GetLastError()==50);
    {MemStatus before,after;void *busy;before.length=sizeof(before);GlobalMemoryStatus(&before);
     busy=VirtualAlloc(0,before.avail-4096,0x3000,4);SetLastError(123);
     check("process.load_oom_returns_failure_and_error",busy&&CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",L"l",0,0,0,0,0,0,0,&i)==0&&GetLastError()==8);
     after.length=sizeof(after);GlobalMemoryStatus(&after);check("process.load_oom_rolls_back_ram",after.avail==4096);
     if(busy)VirtualFree(busy,0,0x8000);}
    ResetEvent(ready);SetLastError(123);
    check("process.invalid_output_rolls_back_loaded_child",CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",L"l",0,0,0,0,0,0,0,(void*)0xDEAD0000)==0&&GetLastError()==87);
    Sleep(50);check("process.failed_commit_never_executes_child",WaitForSingleObject(ready,0)==258);
    check("process.create_suspended",launch(&i,L"d",4));
    check("process.primary_thread_still_active",GetExitCodeThread(i.thread,&code)&&code==259&&WaitForSingleObject(i.thread,0)==258);
    check("process.duplicate_event_to_child",DuplicateHandle(GetCurrentProcess(),ready,i.process,&alias,0,0,2));
    data[0]=alias;data[1]=ready;data[2]=GetCurrentProcessId();data[3]=0;data[4]=i.tid;data[5]=i.pid;
    check("process.publish_duplicate",write_data(data));
    check("process.resume_primary_previous_count",ResumeThread(i.thread)==1);
    check("process.transferred_event_signal",WaitForSingleObject(ready,5000)==0);
    check("process.command_line_ids_no_inheritance_and_tls",waited_code(&i,77));
    check("process.parent_tls_unchanged",TlsGetValue(slot)==0xAABBCCDD);close_info(&i);
    check("process.create_running_child",launch(&i,L"l",0));check("process.child_runs_while_parent_waits",WaitForSingleObject(ready,5000)==0);
    check("process.remote_suspend_counts",SuspendThread(i.thread)==0&&SuspendThread(i.thread)==1);
    Sleep(50);ResetEvent(tick);check("process.suspended_child_stops_progress",WaitForSingleObject(tick,30)==258);
    check("process.remote_resume_counts",ResumeThread(i.thread)==2&&ResumeThread(i.thread)==1);
    check("process.resumed_child_progress",WaitForSingleObject(tick,5000)==0);
    check("process.remote_terminate_primary",TerminateThread(i.thread,33)!=0&&waited_code(&i,33));close_info(&i);
    check("process.create_worker_child",launch(&i,L"w",0));check("process.worker_child_ready",WaitForSingleObject(ready,5000)==0);
    check("process.main_exits_while_worker_keeps_process_alive",WaitForSingleObject(i.thread,5000)==0&&GetExitCodeThread(i.thread,&code)&&code==11&&GetExitCodeProcess(i.process,&code)&&code==259);
    file=CreateFileW(L"\\Flash Disk\\PROCDATA.BIN",0x80000000,3,0,3,0,0);n=0;if(file!=0xFFFFFFFF){ReadFile(file,data,8,&n,0);CloseHandle(file);}
    check("process.duplicate_remote_worker",n==8&&DuplicateHandle(i.process,data[0],GetCurrentProcess(),&alias,0,0,2));
    check("process.control_remote_worker_suspend",SuspendThread(alias)==0);Sleep(50);ResetEvent(tick);check("process.worker_suspension_stops_progress",WaitForSingleObject(tick,30)==258);
    check("process.control_remote_worker_resume",ResumeThread(alias)==1&&WaitForSingleObject(tick,5000)==0);
    check("process.terminate_last_worker_preserves_main_exit_code",TerminateThread(alias,55)&&waited_code(&i,55)&&GetExitCodeThread(i.thread,&code)&&code==11);CloseHandle(alias);close_info(&i);
    check("process.create_termination_target",launch(&i,L"l",4));check("process.remote_terminate_process",TerminateProcess(i.process,66)&&waited_code(&i,66));
    SetLastError(123);check("process.dead_thread_control_error",ResumeThread(i.thread)==0xFFFFFFFF&&GetLastError()==6);close_info(&i);
    h=CreateThread(0,0x40000,parent_worker,0,4,&id);check("process.create_parent_control_target",h!=0);
    check("process.create_parent_controller",launch(&i,L"p",4));data[0]=0;data[1]=0;data[2]=GetCurrentProcessId();data[3]=h;data[4]=i.tid;data[5]=i.pid;write_data(data);
    ResumeThread(i.thread);check("process.child_resumes_and_waits_for_parent_worker",waited_code(&i,77)&&GetExitCodeThread(h,&code)&&code==88);CloseHandle(h);close_info(&i);
    check("process.two_live_children",launch(&i,L"l",4)&&launch(&j,L"l",4));
    check("process.siblings_resume_and_terminate_independently",ResumeThread(i.thread)==1&&ResumeThread(j.thread)==1&&TerminateProcess(i.process,71)&&waited_code(&i,71)&&GetExitCodeProcess(j.process,&code)&&code==259&&TerminateProcess(j.process,72)&&waited_code(&j,72));close_info(&i);close_info(&j);
    check("process.create_child_that_outlives_parent",launch(&i,L"o",4));data[2]=GetCurrentProcessId();write_data(data);ResumeThread(i.thread);check("process.orphan_retains_parent_handle_before_exit",WaitForSingleObject(ready,5000)==0);close_info(&i);
    TlsFree(slot);CloseHandle(ready);CloseHandle(tick);line(failures?"PROCTEST_RESULT FAIL\r\n":"PROCTEST_RESULT PASS\r\n");CloseHandle(report);
    return failures?1:77;
}
