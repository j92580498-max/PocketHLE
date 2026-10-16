/* Standalone native ARM/WinCE test; no CRT or game-specific paths. */
#include "imports.h"
typedef unsigned int U32;
typedef unsigned short W16;
typedef struct { U32 length,load,total,avail,page_total,page_avail,virtual_total,virtual_avail; } MemStatus;
static U32 report, checks, failures, io_failed;
static W16 report_path[260];
static char line_buffer[256];
void *memset(void *p, int v, unsigned int n) { unsigned char *s=p; while(n--) *s++=(unsigned char)v; return p; }
void *memcpy(void *d,const void *s,unsigned int n) { unsigned char *a=d; const unsigned char *b=s; while(n--) *a++=*b++; return d; }
static U32 text(char *out, const char *in) { U32 n=0; while(in[n]) {out[n]=in[n]; n++;} return n; }
static U32 hex(char *out,U32 v) { U32 i; const char *digits="0123456789ABCDEF"; for(i=0;i<8;i++) out[i]=digits[(v >> (28-4*i))&15]; return 8; }
static void emit(const char *s,U32 len) { U32 written=0; if(!WriteFile(report,s,len,&written,0) || written!=len) io_failed=1; }
static void note(const char *s) { U32 n=text(line_buffer,s); line_buffer[n++]='\r';line_buffer[n++]='\n';emit(line_buffer,n); }
static void check(const char *name,U32 ok) { U32 n=0;checks++;if(!ok) failures++;n+=text(line_buffer+n,ok?"PASS ":"FAIL ");n+=text(line_buffer+n,name);line_buffer[n++]='\r';line_buffer[n++]='\n';emit(line_buffer,n); }
static MemStatus status(const char *name) {
    MemStatus s; U32 n=0; memset(&s,0,sizeof(s)); s.length=sizeof(s); GlobalMemoryStatus(&s);
    n+=text(line_buffer+n,"MEASURE ");n+=text(line_buffer+n,name);n+=text(line_buffer+n," total=0x");n+=hex(line_buffer+n,s.total);n+=text(line_buffer+n," avail=0x");n+=hex(line_buffer+n,s.avail);n+=text(line_buffer+n," virtual_free=0x");n+=hex(line_buffer+n,s.virtual_avail);line_buffer[n++]='\r';line_buffer[n++]='\n';emit(line_buffer,n);return s;
}
static U32 worker(void *arg) { volatile U32 stack_words[256]; U32 i; for(i=0;i<256;i++) stack_words[i]=i^(U32)arg;SetLastError(42);return stack_words[255]^255^(U32)arg^77; }
static void test_virtual(void) {
    MemStatus before=status("virtual.before"), after;
    void *p=VirtualAlloc(0,0x100000,0x2000,4);check("virtual.reserve",p!=0);if(!p)return;
    after=status("virtual.reserved");check("virtual.reserve_no_physical_charge",after.avail==before.avail);check("virtual.reserve_reduces_address_space",after.virtual_avail+0x100000==before.virtual_avail);
    check("virtual.commit",VirtualAlloc(p,0x10000,0x1000,4)==p);
    after=status("virtual.committed");check("virtual.commit_16_pages",after.avail+0x10000==before.avail);
    ((volatile U32*)p)[0]=0xCAFE1234;check("virtual.recommit",VirtualAlloc(p,0x10000,0x1000,4)==p);check("virtual.recommit_preserves_data",((volatile U32*)p)[0]==0xCAFE1234);
    after=status("virtual.recommitted");check("virtual.recommit_no_double_charge",after.avail+0x10000==before.avail);
    check("virtual.decommit",VirtualFree(p,0x8000,0x4000)!=0);after=status("virtual.decommitted");check("virtual.decommit_refunds_8_pages",after.avail+0x8000==before.avail);
    check("virtual.release",VirtualFree(p,0,0x8000)!=0);after=status("virtual.released");check("virtual.release_restores_physical",after.avail==before.avail);check("virtual.release_restores_address_space",after.virtual_avail==before.virtual_avail);
    SetLastError(123);p=VirtualAlloc(0,4096,0x3000,0xDEAD);check("virtual.invalid_protection_error",p==0&&GetLastError()==87);
}
static void test_local(void) {
    MemStatus before=status("local.before"),after;void *p=LocalAlloc(0x40,0x10000);check("local.allocate",p!=0);if(!p)return;
    check("local.zero_fill",((volatile U32*)p)[0]==0);after=status("local.allocated");check("local.physical_charge",after.avail+0x10000<=before.avail);
    check("local.free",LocalFree(p)==0);after=status("local.freed");check("local.free_restores_ram",after.avail==before.avail);
}
static void test_heap_errors(void) {
    MemStatus before=status("heap_errors.before"),after;U32 heap=GetProcessHeap();void *p=LocalAlloc(0x40,16),*q;
    check("heap_errors.allocate",p!=0);if(!p)return;((volatile U32*)p)[0]=0x5A123456;
    SetLastError(123);check("heap_errors.invalid_local_free",LocalFree((char*)p+1)==(U32)p+1&&GetLastError()==6);
    SetLastError(123);check("heap_errors.invalid_local_size",LocalSize((char*)p+1)==0&&GetLastError()==6);
    SetLastError(123);check("heap_errors.invalid_heap_free",HeapFree(heap,0,(char*)p+1)==0&&GetLastError()==87);
    SetLastError(123);check("heap_errors.invalid_heap_size",HeapSize(heap,0,(char*)p+1)==0xFFFFFFFF&&GetLastError()==87);
    SetLastError(123);q=HeapReAlloc(heap,0,(char*)p+1,32);check("heap_errors.invalid_realloc",q==0&&GetLastError()==87);
    SetLastError(123);q=LocalReAlloc(p,0x80000000,0x40);check("heap_errors.oom_realloc_preserves_block",q==0&&GetLastError()==8&&LocalSize(p)==16&&((volatile U32*)p)[0]==0x5A123456);
    q=LocalReAlloc(p,32,0x40);check("heap_errors.realloc_copies_and_zeros_tail",q!=0&&((volatile U32*)q)[0]==0x5A123456&&((volatile U32*)q)[7]==0);
    if(q)p=q;check("heap_errors.free",LocalFree(p)==0);
    SetLastError(123);check("heap_errors.double_free",LocalFree(p)==(U32)p&&GetLastError()==6);
    after=status("heap_errors.after");check("heap_errors.ram_restored",before.avail==after.avail);
}
/* Use the CE inline TLS window as well as the exported API. */
static volatile U32 *tls_inline(void) { return *(volatile U32 **)(0xFFFFC800u); }
static U32 tls_index, tls_ready, tls_gate;
static volatile U32 tls_done;
static U32 tls_worker(void *arg) {
    U32 tag=(U32)arg,i,good=1;
    if(TlsGetValue(tls_index)!=0||tls_inline()[tls_index]!=0)good=0;
    for(i=0;i<6;i++) {
        U32 value=0xAA000000u|tag|i*256;
        SetLastError(0xB0000000u|tag);
        if(!TlsSetValue(tls_index,value)||GetLastError()!=(0xB0000000u|tag))good=0;
        tls_inline()[tls_index]=value^0x12345678u;
        Sleep(0);
        if(GetLastError()!=(0xB0000000u|tag)||tls_inline()[tls_index]!=(value^0x12345678u))good=0;
        if(TlsGetValue(tls_index)!=(value^0x12345678u)||GetLastError()!=0)good=0;
    }
    tls_done++;return good?77:1;
}
static U32 tls_reuse_worker(void *arg) {
    U32 good=1;(void)arg;
    if(TlsGetValue(tls_index)!=0)good=0;
    tls_inline()[tls_index]=0xCAFEBABE;
    SetEvent(tls_ready);
    if(WaitForSingleObject(tls_gate,5000)!=0)good=0;
    if(TlsGetValue(tls_index)!=0||tls_inline()[tls_index]!=0)good=0;
    return good?77:1;
}
static void test_tls(void) {
    U32 indices[64],i,good=1,h[2],code=0,id=0,reused;
    MemStatus before=status("tls.before"),after;
    SetLastError(123);tls_index=TlsAlloc();
    check("tls.allocate_preserves_error",tls_index==0&&GetLastError()==123);
    if(tls_index==0xFFFFFFFF)return;
    check("tls.zero_value_clears_error",TlsGetValue(tls_index)==0&&GetLastError()==0);
    SetLastError(123);check("tls.set_preserves_error",TlsSetValue(tls_index,0x11223344)&&GetLastError()==123);
    check("tls.api_write_visible_inline",tls_inline()[tls_index]==0x11223344);
    tls_inline()[tls_index]=0x1234ABCD;SetLastError(123);
    check("tls.inline_write_visible_to_api",TlsGetValue(tls_index)==0x1234ABCD&&GetLastError()==0);
    tls_done=0;h[0]=CreateThread(0,0x40000,tls_worker,(void*)1,4,&id);h[1]=CreateThread(0,0x40000,tls_worker,(void*)2,4,&id);
    if(!h[0]||!h[1])good=0;
    if(h[0])ResumeThread(h[0]);if(h[1])ResumeThread(h[1]);
    for(i=0;i<64&&tls_done<2&&good;i++) {
        SetLastError(0xAABBCCDD);Sleep(0);
        if(GetLastError()!=0xAABBCCDD||tls_inline()[tls_index]!=0x1234ABCD)good=0;
    }
    for(i=0;i<2;i++)if(h[i]){if(WaitForSingleObject(h[i],5000)!=0||!GetExitCodeThread(h[i],&code)||code!=77)good=0;CloseHandle(h[i]);}
    check("tls.main_and_two_workers_keep_values_and_errors",good&&tls_done==2);
    check("tls.main_value_survives_worker_exit",TlsGetValue(tls_index)==0x1234ABCD);
    tls_ready=CreateEventW(0,1,0,0);tls_gate=CreateEventW(0,1,0,0);
    h[0]=CreateThread(0,0x40000,tls_reuse_worker,0,4,&id);good=tls_ready&&tls_gate&&h[0];
    if(h[0])ResumeThread(h[0]);
    if(good&&WaitForSingleObject(tls_ready,5000)!=0)good=0;
    check("tls.free_allocated_slot",TlsFree(tls_index)!=0);
    SetLastError(123);check("tls.double_free_error",TlsFree(tls_index)==0&&GetLastError()==87);
    reused=TlsAlloc();check("tls.reuse_zeroes_current_thread",reused==tls_index&&TlsGetValue(reused)==0);
    TlsSetValue(tls_index,0x55556666);
    if(tls_gate)SetEvent(tls_gate);
    if(h[0]){if(WaitForSingleObject(h[0],5000)!=0||!GetExitCodeThread(h[0],&code)||code!=77)good=0;CloseHandle(h[0]);}
    if(tls_ready)CloseHandle(tls_ready);if(tls_gate)CloseHandle(tls_gate);
    check("tls.reuse_zeroes_parked_worker_and_new_thread",good);
    check("tls.reuse_preserves_main_value",TlsGetValue(tls_index)==0x55556666);
    check("tls.free_reused_slot",TlsFree(tls_index)!=0);
    SetLastError(123);check("tls.ce_range_only_set",TlsSetValue(63,77)!=0&&GetLastError()==123);
    check("tls.ce_range_only_get",TlsGetValue(63)==77&&GetLastError()==0);
    SetLastError(123);check("tls.free_unallocated_error",TlsFree(63)==0&&GetLastError()==87);
    good=1;for(i=0;i<64;i++){indices[i]=TlsAlloc();if(indices[i]!=i||TlsGetValue(indices[i])!=0)good=0;}
    check("tls.64_slots_zero_initialized",good);
    SetLastError(123);check("tls.exhaustion_error",TlsAlloc()==0xFFFFFFFF&&GetLastError()==8);
    SetLastError(123);check("tls.invalid_get_error",TlsGetValue(64)==0&&GetLastError()==87);
    SetLastError(123);check("tls.invalid_set_error",TlsSetValue(0xFFFFFFFF,77)==0&&GetLastError()==87);
    SetLastError(123);check("tls.invalid_free_error",TlsFree(64)==0&&GetLastError()==87);
    good=1;for(i=0;i<64;i++)if(indices[i]!=0xFFFFFFFF&&!TlsFree(indices[i]))good=0;
    check("tls.free_all_slots",good);
    after=status("tls.after");check("tls.worker_stacks_and_ram_refunded",after.avail==before.avail);
}
static void test_wait_errors(void) {
    U32 ev=CreateEventW(0,0,1,0),handles[2]={ev,ev};
    SetLastError(123);check("wait.zero_count_error",WaitForMultipleObjects(0,handles,0,0)==0xFFFFFFFF&&GetLastError()==87);
    SetLastError(123);check("wait.excess_count_error",WaitForMultipleObjects(65,handles,0,0)==0xFFFFFFFF&&GetLastError()==87);
    SetLastError(123);check("wait.null_array_error",WaitForMultipleObjects(1,0,0,0)==0xFFFFFFFF&&GetLastError()==87);
    SetLastError(123);check("wait.unmapped_array_error",WaitForMultipleObjects(1,(U32*)0xDEAD0000,0,0)==0xFFFFFFFF&&GetLastError()==87);
    SetLastError(123);check("wait.duplicate_object_error",WaitForMultipleObjects(2,handles,1,0)==0xFFFFFFFF&&GetLastError()==87);
    check("wait.failure_does_not_consume_event",ev&&WaitForSingleObject(ev,0)==0);
    handles[0]=0;SetLastError(123);check("wait.null_handle_error",WaitForMultipleObjects(1,handles,0,0)==0xFFFFFFFF&&GetLastError()==6);
    SetLastError(123);check("wait.single_invalid_handle_error",WaitForSingleObject(0xFFFFFFFF,0)==0xFFFFFFFF&&GetLastError()==6);
    if(ev)CloseHandle(ev);SetLastError(123);check("wait.closed_handle_error",WaitForSingleObject(ev,0)==0xFFFFFFFF&&GetLastError()==6);
}
static void test_threads(void) {
    MemStatus before=status("threads.before"),after;U32 i,good=1;
    for(i=0;i<32;i++) {
        U32 id=0,exit_code=0,h=CreateThread(0,0x40000,worker,(void*)i,4,&id);if(!h){good=0;break;}
        if(i==0){after=status("threads.suspended_stack");check("threads.stack_charged",after.avail+0x40000<=before.avail);}
        SetLastError(0xAABBCCDD);if(ResumeThread(h)!=1||WaitForSingleObject(h,5000)!=0){good=0;CloseHandle(h);break;}
        if(GetLastError()!=0xAABBCCDD)good=0;
        if(!GetExitCodeThread(h,&exit_code)||exit_code!=77||id==0)good=0;
        after=status(i==0?"threads.first_exit":"threads.exit");if(after.avail!=before.avail)good=0;
        if(!CloseHandle(h))good=0;
    }
    check("threads.32_cycles_exit_status_and_error_isolation",good&&i==32);after=status("threads.after");check("threads.all_stacks_refunded",after.avail==before.avail);
}
static void test_processes(void) {
    U32 info[4]={0},code=0,event=CreateEventW(0,1,0,L"PocketHLE.PROC.ORPHAN_DONE"),i;
    MemStatus before=status("processes.before"),after;
    check("processes.suite_launch",CreateProcessW(L"\\SD Card\\GZRT999999\\proctest.exe",L"suite",0,0,0,0,0,0,0,info)!=0);
    check("processes.suite_exit_and_report",info[0]&&WaitForSingleObject(info[0],15000)==0&&GetExitCodeProcess(info[0],&code)&&code==77);
    check("processes.orphan_completed",event&&WaitForSingleObject(event,5000)==0);
    if(info[0])CloseHandle(info[0]);if(info[1])CloseHandle(info[1]);if(event)CloseHandle(event);
    for(i=0;i<200;i++){after.length=sizeof(after);GlobalMemoryStatus(&after);if(after.avail==before.avail)break;Sleep(10);}
    after=status("processes.after");check("processes.all_children_ram_refunded",after.avail==before.avail);
}
static void test_dlls(void) {
    MemStatus before=status("dll.before"),after;U32 i,good=1;
    SetLastError(123);check("dll.missing_module_error",LoadLibraryW(L"absent-ramtest.dll")==0&&GetLastError()==126);
    SetLastError(123);check("dll.null_path_error",LoadLibraryW(0)==0&&GetLastError()==87);
    for(i=0;i<24;i++) {
        U32 a=LoadLibraryW(L"ramprobe.dll"),b;U32 (*probe)(void);
        if(!a){good=0;break;}after=status(i==0?"dll.loaded":"dll.reload");if(after.avail>=before.avail||after.avail+0x20000<=before.avail)good=0;
        b=LoadLibraryW(L"ramprobe.dll");if(b!=a)good=0;
        if(i==0){SetLastError(123);check("dll.missing_export_error",GetProcAddressA(a,"MissingExport")==0&&GetLastError()==127);}
        probe=(U32(*)(void))GetProcAddressA(a,"RamProbe");if(!probe||probe()!=77)good=0;
        if(!FreeLibrary(a))good=0;{MemStatus same=status("dll.one_reference");if(same.avail!=after.avail)good=0;}
        if(!FreeLibrary(b))good=0;after=status("dll.unloaded");if(after.avail!=before.avail)good=0;
    }
    check("dll.24_cycles_references_exports_and_refund",good&&i==24);
    SetLastError(123);{U32 h=LoadLibraryW(L"ramreject.dll");U32 e=GetLastError();check("dll.rejected_attach_returns_null_and_error",h==0&&e==1114);if(h)FreeLibrary(h);}
    after=status("dll.rejected_attach");check("dll.rejected_attach_refunds_pages",after.avail==before.avail);
}
static void test_image_paging(void) {
    MemStatus before=status("paging.before"),loaded,read,write,fetch,after;
    U32 h=LoadLibraryW(L"pageprobe.dll");volatile U32 *data;U32 (*far_code)(void);
    check("paging.load",h!=0);if(!h)return;
    loaded=status("paging.loaded");check("paging.loader_commits_only_entry_page",loaded.avail+4096==before.avail);
    data=(volatile U32*)GetProcAddressA(h,"PageData");far_code=(U32(*)(void))GetProcAddressA(h,"FarCode");
    check("paging.exports",data&&far_code);if(!data||!far_code){FreeLibrary(h);return;}
    check("paging.first_read_restores_initialized_bytes",data[0]==0x1234ABCD);
    read=status("paging.read");check("paging.read_charges_one_page",read.avail+4096==loaded.avail);
    check("paging.cold_zero_fill",data[3*1024]==0);
    data[3*1024]=0xCAFEBABE;check("paging.write_preserves_dirty_data",data[3*1024]==0xCAFEBABE);
    write=status("paging.write");check("paging.zero_fill_charges_one_page",write.avail+4096==read.avail);
    data[3*1024]=0x87654321;after=status("paging.repeated");check("paging.repeated_access_no_double_charge",after.avail==write.avail);
    check("paging.cold_code_executes",far_code()==77);
    fetch=status("paging.fetch");check("paging.fetch_charges_one_page",fetch.avail+4096==write.avail);
    check("paging.second_initialized_page",data[2*1024]==0xDEAD1234);
    after=status("paging.partial");check("paging.untouched_tail_stays_uncommitted",after.avail+5*4096==before.avail);
    check("paging.free",FreeLibrary(h)!=0);after=status("paging.freed");check("paging.free_refunds_only_committed_pages",after.avail==before.avail);
    h=LoadLibraryW(L"pageprobe.dll");data=h?(volatile U32*)GetProcAddressA(h,"PageData"):0;
    check("paging.reload_discards_dirty_backing",data&&data[0]==0x1234ABCD&&data[3*1024]==0);
    if(h)FreeLibrary(h);after=status("paging.reloaded_and_freed");check("paging.reload_refunds_pages",after.avail==before.avail);
}
static void test_native_dependencies(void) {
    MemStatus before=status("dependencies.before"),after;U32 root,peer,leaf;U32 (*value)(void);
    U32 trace=CreateFileW(L"\\Flash Disk\\DEPTEST.TXT",0x40000000,3,0,2,0x80,0);if(trace!=0xFFFFFFFF)CloseHandle(trace);
    root=LoadLibraryW(L"deproot.dll");check("dependencies.root_attach_after_leaf",root!=0);
    if(root){value=(U32(*)(void))GetProcAddressA(root,"Value");check("dependencies.direct_native_import",value&&value()==77);}
    peer=LoadLibraryW(L"deppeer.dll");check("dependencies.ordinal_import",peer!=0);
    if(peer){value=(U32(*)(void))GetProcAddressA(peer,"Value");check("dependencies.ordinal_call",value&&value()==77);}
    if(root)FreeLibrary(root);
    if(peer){value=(U32(*)(void))GetProcAddressA(peer,"Value");check("dependencies.shared_leaf_survives_first_parent",value&&value()==77);FreeLibrary(peer);}
    after=status("dependencies.shared_released");check("dependencies.shared_graph_refunds_ram",after.avail==before.avail);
    root=LoadLibraryW(L"deproot.dll");leaf=LoadLibraryW(L"depleaf.dll");if(root)FreeLibrary(root);
    if(leaf){value=(U32(*)(void))GetProcAddressA(leaf,"Value");check("dependencies.explicit_leaf_reference_survives_parent",value&&value()==77);FreeLibrary(leaf);}else check("dependencies.explicit_leaf_reference_survives_parent",0);
    after=status("dependencies.explicit_released");check("dependencies.explicit_refunds_ram",after.avail==before.avail);
    SetLastError(123);root=LoadLibraryW(L"depmissing.dll");check("dependencies.missing_dll_error",root==0&&GetLastError()==126);if(root)FreeLibrary(root);
    after=status("dependencies.missing_rejected");check("dependencies.missing_dll_rolls_back",after.avail==before.avail);
    SetLastError(123);root=LoadLibraryW(L"depbadexport.dll");check("dependencies.missing_export_error",root==0&&GetLastError()==127);if(root)FreeLibrary(root);
    after=status("dependencies.export_rejected");check("dependencies.missing_export_rolls_back",after.avail==before.avail);
    SetLastError(123);root=LoadLibraryW(L"depreject.dll");check("dependencies.failed_attach_error",root==0&&GetLastError()==1114);if(root)FreeLibrary(root);
    after=status("dependencies.attach_rejected");check("dependencies.failed_attach_rolls_back",after.avail==before.avail);
    root=LoadLibraryW(L"depcyclea.dll");check("dependencies.cycle_load",root!=0);
    if(root){value=(U32(*)(void))GetProcAddressA(root,"Value");check("dependencies.cycle_imports_bound",value&&value()==77);FreeLibrary(root);}
    after=status("dependencies.cycle_released");check("dependencies.cycle_refunds_ram",after.avail==before.avail);
    {
        static char trace_bytes[512];
        static const char expected[]=
            "leaf attach\r\nroot attach\r\npeer attach\r\nroot detach\r\npeer detach\r\nleaf detach\r\n"
            "leaf attach\r\nroot attach\r\nroot detach\r\nleaf detach\r\n"
            "leaf attach\r\nreject attach\r\nleaf detach\r\n"
            "cycle-b attach\r\ncycle-a attach\r\ncycle-a detach\r\ncycle-b detach\r\n";
        U32 read=0,written=0,i,valid=0;
        trace=CreateFileW(L"\\Flash Disk\\DEPTEST.TXT",0xC0000000,3,0,3,0x80,0);
        if(trace!=0xFFFFFFFF) {
            valid=ReadFile(trace,trace_bytes,sizeof(trace_bytes),&read,0)&&read==sizeof(expected)-1;
            for(i=0;valid&&i<read;i++)if(trace_bytes[i]!=expected[i])valid=0;
            SetFilePointer(trace,0,0,2);
            {const char *result=valid?"DEPTEST_RESULT PASS\r\n":"DEPTEST_RESULT FAIL\r\n";
             WriteFile(trace,result,21,&written,0);}
            CloseHandle(trace);
        }
        check("dependencies.callback_order_and_rollback",valid);
    }
}
static U32 life_tls,life_tls_checks,life_tls_bad;
static U32 life_report,life_a,life_b,life_main,life_count,life_bad,life_process_count,life_expected,life_tid,life_process_owner,life_main_detach,life_existing_tid,life_existing_detach,life_existing_started,life_gate;
static void lifecycle_note(U32 module,U32 reason,U32 reserved) {
    U32 old=report,n=0,tid=GetCurrentThreadId(),expected,next,value;report=life_report;
    expected=next=0xF00D0000u;
    if(tid==life_existing_tid)expected=next=0xEEEE0000u;
    else if(tid!=life_main) {
        if(reason==2) { expected=module==life_a?0:(0xAA000000u|tid);next=(module==life_a?0xAA000000u:0xBB000000u)|tid; }
        else if(reason==3) { expected=(module==life_b?0xCC000000u:0xDD000000u)|tid;next=0xDD000000u|tid; }
        else expected=next=0xCC000000u|tid;
    }
    value=TlsGetValue(life_tls);
    if(value!=expected||tls_inline()[life_tls]!=expected||!TlsSetValue(life_tls,next)){life_tls_bad=1;life_bad=1;}
    life_tls_checks++;

    n+=text(line_buffer+n,"DLL base=0x");n+=hex(line_buffer+n,module);n+=text(line_buffer+n," reason=0x");n+=hex(line_buffer+n,reason);n+=text(line_buffer+n," thread=0x");n+=hex(line_buffer+n,tid);n+=text(line_buffer+n," reserved=0x");n+=hex(line_buffer+n,reserved);line_buffer[n++]='\r';line_buffer[n++]='\n';emit(line_buffer,n);
    if(reason==0) {
        if(tid!=life_process_owner||reserved==0||module!=(life_process_count==0?life_b:life_a))life_bad=1;
        life_process_count++;
        if(life_process_count==2){note(life_bad?"DLLTEST_RESULT FAIL":"DLLTEST_RESULT PASS");CloseHandle(life_report);}
    } else if(tid==life_existing_tid) {
        if(reason!=3||reserved!=0||module!=(life_existing_detach==0?life_b:life_a))life_bad=1;
        life_existing_detach++;
    } else if(reason==3&&tid==life_main) {
        if(reserved!=0||module!=(life_main_detach==0?life_b:life_a))life_bad=1;
        life_main_detach++;
    } else {
        U32 position=life_count%4;
        if(reserved!=0||tid!=life_tid||reason!=(position<2?2:3)||module!=((position==0||position==3)?life_a:life_b))life_bad=1;
        life_count++;
    }
    SetLastError(0xBADCA11); // The continuation must restore the interrupted context.
    report=old;
}
static U32 lifecycle_worker(void *arg) {
    if(life_count!=life_expected||GetLastError()!=0)life_bad=1;
    if(TlsGetValue(life_tls)!=(0xBB000000u|GetCurrentThreadId())){life_tls_bad=1;life_bad=1;}
    TlsSetValue(life_tls,0xCC000000u|GetCurrentThreadId());
    if((U32)arg==1){ExitThread(88);return 99;}
    return 77;
}
static U32 existing_lifecycle_worker(void *arg) {
    (void)arg;TlsSetValue(life_tls,0xEEEE0000u);life_existing_started=1;
    if(WaitForSingleObject(life_gate,0xFFFFFFFF)!=0)life_bad=1;
    return 66;
}
static void test_lifecycle(void) {
    U32 old=report,i,existing=0;void (*configure)(void (*)(U32,U32,U32));
    life_report=CreateFileW(L"\\Flash Disk\\DLLTEST.TXT",0x40000000,0,0,2,0x80,0);
    check("dll_lifecycle.report_open",life_report!=0xFFFFFFFF);if(life_report==0xFFFFFFFF)return;
    life_main=GetCurrentThreadId();life_process_owner=life_main;
    life_tls=TlsAlloc();if(life_tls==0xFFFFFFFF){life_bad=1;return;}TlsSetValue(life_tls,0xF00D0000u);
    life_gate=CreateEventW(0,1,0,0);existing=CreateThread(0,0x40000,existing_lifecycle_worker,0,4,&life_existing_tid);
    if(life_gate&&existing){ResumeThread(existing);Sleep(0);}else life_bad=1;
    check("dll_lifecycle.existing_worker_started_before_load",life_existing_started&&!life_bad);
    life_a=LoadLibraryW(L"ramprobe.dll");life_b=LoadLibraryW(L"ramprobe2.dll");
    check("dll_lifecycle.two_distinct_modules",life_a&&life_b&&life_a!=life_b);if(!life_a||!life_b)return;
    configure=(void(*)(void(*)(U32,U32,U32)))GetProcAddressA(life_a,"Configure");if(!configure){life_bad=1;return;}configure(lifecycle_note);
    configure=(void(*)(void(*)(U32,U32,U32)))GetProcAddressA(life_b,"Configure");if(!configure){life_bad=1;return;}configure(lifecycle_note);
    check("dll_lifecycle.no_retroactive_main_attach",life_count==0);
    if(existing){U32 code=0;SetEvent(life_gate);if(WaitForSingleObject(existing,5000)!=0||!GetExitCodeThread(existing,&code)||code!=66)life_bad=1;CloseHandle(existing);}
    if(life_gate)CloseHandle(life_gate);
    check("dll_lifecycle.existing_worker_detach_without_attach",life_count==0&&life_existing_detach==2&&!life_bad);
    for(i=0;i<2;i++) {
        U32 h,id=0,code=0;life_expected=i*4+2;h=CreateThread(0,0x40000,lifecycle_worker,(void*)i,4,&id);life_tid=id;
        if(!h||life_count!=i*4){life_bad=1;break;}
        if(ResumeThread(h)!=1||WaitForSingleObject(h,5000)!=0||!GetExitCodeThread(h,&code)||code!=(i==0?77:88))life_bad=1;
        if(life_count!=(i+1)*4)life_bad=1;CloseHandle(h);
    }
    check("dll_lifecycle.return_and_exitthread_notifications",i==2&&!life_bad&&life_count==8);
    check("tls.dll_attach_and_detach_keep_thread_storage",!life_tls_bad&&life_tls_checks==10&&TlsGetValue(life_tls)==0xF00D0000u);
    {U32 h,id=0;h=CreateThread(0,0x40000,lifecycle_worker,0,4,&id);if(!h||!TerminateThread(h,9))life_bad=1;else CloseHandle(h);}
    check("dll_lifecycle.forced_suspended_thread_has_no_callbacks",life_count==8&&!life_bad);
    // Both DLLs remain resident until process exit. Their callbacks finish DLLTEST.TXT.
    report=life_report;note("THREAD_PHASE_COMPLETE; waiting for process detach");report=old;
}
#ifdef TEST_LAST_WORKER_EXIT
static U32 last_lifecycle_worker(void *arg) {
    (void)arg;
    if(life_main_detach!=2||life_count!=10||TlsGetValue(life_tls)!=(0xBB000000u|GetCurrentThreadId()))life_bad=1;
    TlsSetValue(life_tls,0xCC000000u|GetCurrentThreadId());
    return 33;
}
#endif
/* Mix resource types: isolated tests do not catch interactions between owners. */
static void test_mixed_lifetimes(void) {
    MemStatus before=status("mixed.before"),after;U32 i,good=1;
    for(i=0;i<64;i++) {
        U32 event=0,alias=0,mutex=0,thread=0,module=0,id=0,code=0;
        void *block=LocalAlloc(0x40,8192),*pages=VirtualAlloc(0,16384,0x3000,4);
        event=CreateEventW(0,0,0,0);mutex=CreateMutexW(0,1,0);
        module=LoadLibraryW(L"ramprobe.dll");thread=CreateThread(0,0x40000,worker,(void*)i,4,&id);
        if(!block||!pages||!event||!mutex||!module||!thread)good=0;
        if(event) {
            if(!DuplicateHandle(GetCurrentProcess(),event,GetCurrentProcess(),&alias,0,0,2))good=0;
            if(!CloseHandle(event))good=0;
            SetLastError(123);if(SetEvent(event)||GetLastError()!=6)good=0;
            if(alias&&(!SetEvent(alias)||WaitForSingleObject(alias,0)!=0||!CloseHandle(alias)))good=0;
        }
        if(mutex&&(!ReleaseMutex(mutex)||!CloseHandle(mutex)))good=0;
        if(thread){if(ResumeThread(thread)!=1||WaitForSingleObject(thread,5000)!=0||!GetExitCodeThread(thread,&code)||code!=77)good=0;if(!CloseHandle(thread))good=0;}
        if(module&&!FreeLibrary(module))good=0;
        if(block&&LocalFree(block)!=0)good=0;
        if(pages&&!VirtualFree(pages,0,0x8000))good=0;
        after.length=sizeof(after);GlobalMemoryStatus(&after);
        if(after.avail!=before.avail||after.virtual_avail!=before.virtual_avail)good=0;
        if(!good)break;
    }
    check("mixed.64_cycles_alloc_event_mutex_thread_dll",good&&i==64);
    after=status("mixed.after");check("mixed.exact_ram_and_address_space_refund",after.avail==before.avail&&after.virtual_avail==before.virtual_avail);
}
static void test_process_repetition(void) {
    MemStatus before=status("process_cycles.before"),after;U32 i,good=1,j;
    for(i=0;i<8;i++) {
        U32 info[4]={0},code=0;
        if(!CreateProcessW(L"\\SD Card\\GZRT999999\\procworker.exe",L"c",0,0,0,0,0,0,0,info)) {good=0;break;}
        if(WaitForSingleObject(info[0],5000)!=0||!GetExitCodeProcess(info[0],&code)||code!=77)good=0;
        if(!CloseHandle(info[1])||!CloseHandle(info[0]))good=0;
        for(j=0;j<200;j++){after.length=sizeof(after);GlobalMemoryStatus(&after);if(after.avail==before.avail)break;Sleep(10);}
        if(after.avail!=before.avail)good=0;
        if(!good)break;
    }
    check("process_cycles.8_launch_exit_close_reclaim",good&&i==8);
    after=status("process_cycles.after");check("process_cycles.exact_ram_refund",after.avail==before.avail);
}
static void test_division(U32 store,U32 ram,U32 page) {
    MemStatus before=status("division.before"),after;U32 s=0,r=0,p=0,result;
    result=SetSystemMemoryDivision(store+16);check("division.shift_free_pages",result==0);
    if(result==0) {
        check("division.read_changed",GetSystemMemoryDivision(&s,&r,&p)!=0&&s==store+16&&r+16==ram&&p==page);
        after=status("division.shifted");check("division.status_total_and_free_follow",after.total+16*page==before.total&&after.avail+16*page==before.avail);
    }
    check("division.restore",SetSystemMemoryDivision(store)==0);
    after=status("division.restored");check("division.restore_status",after.total==before.total&&after.avail==before.avail);
    SetLastError(123);result=SetSystemMemoryDivision(store+ram-1);check("division.reject_live_program_pages",result==3&&GetLastError()==8);
    after=status("division.refused");check("division.refusal_preserves_status",after.total==before.total&&after.avail==before.avail);
    SetLastError(123);result=SetSystemMemoryDivision(store+ram);check("division.invalid_size_error",result==3&&GetLastError()==87);
    check("division.final_restore",SetSystemMemoryDivision(store)==0);
}
__attribute__((section(".text.entry"))) U32 entry(void) {
    U32 n=0,store=0,ram=0,page=0;MemStatus s;
    // Stabilize the diagnostic's own pages before measuring each operation.
    // Separate paging fixtures below retain untouched pages for lazy-load tests.
    {extern char __image_start[],__image_end[]; volatile unsigned char sink=0;
     const volatile unsigned char *p=(const volatile unsigned char*)__image_start;
     while(p<(const volatile unsigned char*)__image_end){sink^=*p;p+=4096;}(void)sink;}

    {const W16 *name=L"\\Flash Disk\\RAMTEST.TXT";while(name[n]&&n<259){report_path[n]=name[n];n++;}report_path[n]=0;}
    report=CreateFileW(report_path,0x40000000,0,0,2,0x80,0);
    if(report==0xFFFFFFFF){MessageBoxW(0,L"Cannot write RAMTEST.TXT.",L"PocketHLE RAM test",0);return 2;}
    note("PocketHLE native ARM RAM + image paging + TLS + processes test v8");
    if(!GetSystemMemoryDivision(&store,&ram,&page)){check("gizmondo.profile_present",0);note("STOP: import this ZIP as a Gizmondo title; no RAM profile configured.");}
    else {
        check("gizmondo.profile_present",1);s=status("baseline");check("status.structure_and_page_size",s.length==32&&page==4096);check("status.total_matches_division",s.total==ram*page);check("status.no_swap",s.page_total==0&&s.page_avail==0);check("status.private_slot",s.virtual_total==0x2000000);
        test_virtual();test_local();test_heap_errors();test_tls();test_wait_errors();test_threads();test_processes();test_dlls();test_mixed_lifetimes();test_process_repetition();test_division(store,ram,page);test_native_dependencies();test_image_paging();test_lifecycle();
    }
    {U32 n=0;n+=text(line_buffer+n,(failures||io_failed)?"RAMTEST_RESULT FAIL checks=0x":"RAMTEST_RESULT PASS checks=0x");n+=hex(line_buffer+n,checks);n+=text(line_buffer+n," failures=0x");n+=hex(line_buffer+n,failures);line_buffer[n++]='\r';line_buffer[n++]='\n';emit(line_buffer,n);}
    CloseHandle(report);
    MessageBoxW(0,(failures||io_failed)?L"FAIL: see RAMTEST.TXT.":L"PASS: RAM and lifecycle checks. Close, then check DLLTEST.TXT.",L"PocketHLE RAM test",0);
#ifdef TEST_EXIT_PROCESS
    ExitProcess(77);
#elif defined(TEST_LAST_WORKER_EXIT)
    {U32 id=0,h=CreateThread(0,0x40000,last_lifecycle_worker,0,4,&id);life_tid=id;life_process_owner=id;
    if(!h)return 2;ResumeThread(h);ExitThread(11);}
#endif
    return failures||io_failed;
}
