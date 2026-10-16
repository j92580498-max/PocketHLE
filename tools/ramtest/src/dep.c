typedef unsigned int U32;
static U32 initialized;
extern U32 dep_call(void);
extern U32 dep_second(void);
extern U32 CreateFileW(const unsigned short*,U32,U32,void*,U32,U32,U32);
extern U32 SetFilePointer(U32,U32,U32*,U32);
extern U32 WriteFile(U32,const void*,U32,U32*,void*);
extern U32 CloseHandle(U32);
static void trace(U32 reason) {
    const char *s;U32 n=0,written=0,h;
#if KIND==0
    s=reason==1?"leaf attach\r\n":"leaf detach\r\n";
#elif KIND==1
    s=reason==1?"root attach\r\n":"root detach\r\n";
#elif KIND==2
    s=reason==1?"peer attach\r\n":"peer detach\r\n";
#elif KIND==3
    s=reason==1?"reject attach\r\n":"reject detach\r\n";
#elif KIND==4
    s=reason==1?"cycle-a attach\r\n":"cycle-a detach\r\n";
#else
    s=reason==1?"cycle-b attach\r\n":"cycle-b detach\r\n";
#endif
    while(s[n])n++;
    h=CreateFileW(L"\\Flash Disk\\DEPTEST.TXT",0xC0000000,3,0,4,0x80,0);
    if(h==0xFFFFFFFF)return;
    SetFilePointer(h,0,0,2);WriteFile(h,s,n,&written,0);CloseHandle(h);
}
__attribute__((section(".text.entry"))) U32 entry(U32 module,U32 reason,void *reserved) {
    (void)module;(void)reserved;
    if(reason==1) {
#if KIND==1 || KIND==2 || KIND==3 || KIND==4
        if(dep_call()!=77)return 0;
#endif
        initialized=1;trace(reason);
#if KIND==3
        return 0;
#endif
    }else if(reason==0) {
#if KIND==1 || KIND==2 || KIND==3 || KIND==4
        // Dependencies must still be initialized and mapped on importer detach.
        if(dep_call()!=77)return 0;
#endif
        trace(reason);initialized=0;
    }
    return 1;
}
U32 Value(void) {
#if KIND==1 || KIND==2 || KIND==3 || KIND==4
    return initialized?dep_call():0;
#elif KIND==5
    return initialized&&dep_call()==5?77:0;
#else
    return initialized?77:0;
#endif
}
U32 Helper(void) { return 5; }
