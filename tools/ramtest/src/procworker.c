#include "imports.h"
typedef unsigned int U32;
typedef unsigned short W16;
static U32 ready,tick;
static U32 read_data(U32 *data,U32 bytes) { U32 n=0,h=CreateFileW(L"\\Flash Disk\\PROCDATA.BIN",0x80000000,3,0,3,0,0);if(h==0xFFFFFFFF)return 0;ReadFile(h,data,bytes,&n,0);CloseHandle(h);return n==bytes; }
static U32 loop(void *unused) { (void)unused;for(;;){SetEvent(tick);Sleep(10);}return 0; }
__attribute__((section(".text.entry"))) U32 entry(U32 instance,U32 previous,const W16 *command,U32 show) {
    U32 data[6],h,alias=0,code=0,n=0,id=0,good=1;const W16 *full=GetCommandLineW();
    (void)instance;(void)previous;(void)show;
    if(!command||!full||full[0]!=command[0])return 1;
    if(command[0]=='c')return 77;
    ready=CreateEventW(0,1,0,L"PocketHLE.PROC.READY");tick=CreateEventW(0,0,0,L"PocketHLE.PROC.TICK");
    if(!ready||!tick)return 2;
    switch(command[0]) {
    case 'd':
        if(!read_data(data,sizeof(data)))return 3;
        SetLastError(123);if(SetEvent(data[1])!=0||GetLastError()!=6)good=0;
        SetLastError(123);if(WaitForSingleObject(data[1],0)!=0xFFFFFFFF||GetLastError()!=6)good=0;
        if(GetCurrentThreadId()!=data[4]||GetCurrentProcessId()!=data[5]||TlsGetValue(0)!=0)good=0;
        TlsSetValue(0,0x55556666);if(!SetEvent(data[0]))good=0;
        return good?77:4;
    case 'l':SetEvent(ready);return loop(0);
    case 'w':
        h=CreateThread(0,0x40000,loop,0,4,&id);
        data[0]=h;data[1]=id;
        {U32 file=CreateFileW(L"\\Flash Disk\\PROCDATA.BIN",0x40000000,3,0,2,0,0);if(file==0xFFFFFFFF)return 5;WriteFile(file,data,8,&n,0);CloseHandle(file);if(n!=8)return 6;}
        if(!h||ResumeThread(h)!=1)return 7;SetEvent(ready);ExitThread(11);return 8;
    case 'p':
        if(!read_data(data,sizeof(data)))return 9;
        h=OpenProcess(0,0,data[2]);if(!h)return 10;
        if(!DuplicateHandle(h,data[3],GetCurrentProcess(),&alias,0,0,2)||ResumeThread(alias)!=1)return 12;
        good=WaitForSingleObject(alias,5000)==0&&GetExitCodeThread(alias,&code)&&code==88;
        CloseHandle(alias);CloseHandle(h);return good?77:13;
    case 'o':
        if(!read_data(data,sizeof(data)))return 14;
        h=OpenProcess(0,0,data[2]);if(!h)return 15;SetEvent(ready);
        good=WaitForSingleObject(h,5000)==0&&GetExitCodeProcess(h,&code)&&code==77;CloseHandle(h);
        {U32 file=CreateFileW(L"\\Flash Disk\\ORPHANTEST.TXT",0x40000000,3,0,2,0,0);
         const char *s=good?"ORPHANTEST_RESULT PASS\r\n":"ORPHANTEST_RESULT FAIL\r\n";
         if(file==0xFFFFFFFF)return 16;WriteFile(file,s,24,&n,0);CloseHandle(file);}
        h=CreateEventW(0,1,0,L"PocketHLE.PROC.ORPHAN_DONE");SetEvent(h);return good?77:17;
    default:return 18;
    }
}
