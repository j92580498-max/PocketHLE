#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
void __aeabi_memclr4(void*p,U32 n){volatile unsigned char*b=p;while(n--)*b++=0;}
static U32 (*InternetOpenW)(const W16*,U32,const W16*,const W16*,U32);
static U32 (*InternetConnectW)(U32,const W16*,U32,const W16*,const W16*,U32,U32,U32);
static U32 (*HttpOpenRequestW)(U32,const W16*,const W16*,const W16*,const W16*,const W16**,U32,U32);
static U32 (*HttpAddRequestHeadersW)(U32,const W16*,U32,U32);
static U32 (*HttpSendRequestW)(U32,const W16*,U32,const void*,U32);
static U32 (*HttpQueryInfoW)(U32,U32,void*,U32*,U32*);
static U32 (*InternetQueryDataAvailable)(U32,U32*,U32,U32);
static U32 (*InternetReadFile)(U32,void*,U32,U32*);
static U32 (*InternetCloseHandle)(U32);
static char report[4096];static U32 used,failed;
static void line(const char*s){while(*s&&used<4000)report[used++]=*s++;}
static void hex(U32 n){U32 i;line("0x");for(i=0;i<8;i++)report[used++]="0123456789abcdef"[(n>>(28-4*i))&15];}
static void check(const char*s,U32 ok){line(ok?"PASS ":"FAIL ");line(s);if(!ok){failed++;line(" error=");hex(GetLastError());}line("\r\n");}
#define BIND(name) name=(void*)GetProcAddressA(dll,#name);check(#name ".export",name!=0);if(!name)goto finish
static void run(U32 secure,U32 post){
 U32 s=0,c=0,r=0,n=0,total=0,available=0,code=0,size=4;unsigned char data[4096];W16 length[32];
 line(secure?"HTTPS\r\n":"HTTP\r\n");
 s=InternetOpenW(L"PocketHLE NETTEST",0,0,0,0);check("session.open",s!=0);if(!s)goto cleanup;
 c=InternetConnectW(s,L"example.com",secure?443:80,0,0,3,0,0);check("http.connect",c!=0);if(!c)goto cleanup;
 r=HttpOpenRequestW(c,post?L"POST":L"GET",L"/",L"HTTP/1.1",0,0,0x04000000|(secure?0x00800000:0),0);check("request.open",r!=0);if(!r)goto cleanup;
 check("request.add_headers",HttpAddRequestHeadersW(r,post?L"Content-Type: application/x-www-form-urlencoded\r\n":L"Accept: */*\r\n",0xffffffff,0x20000000));
 check("request.send",HttpSendRequestW(r,0,0xffffffff,post?"test=1":0,post?6:0));
 if(!HttpQueryInfoW(r,19|0x20000000,&code,&size,0)){check("response.status",0);goto cleanup;}
 line("STATUS=");hex(code);line("\r\n");check("response.status",code>=100&&code<=599);
 size=0;if(!HttpQueryInfoW(r,5,0,&size,0)){U32 e=GetLastError();check("content_length.probe",e==122||e==12150);if(e==122&&size<=sizeof(length)){check("content_length.utf16",HttpQueryInfoW(r,5,length,&size,0));}}
 while(1){if(!InternetQueryDataAvailable(r,&available,0,0)){check("response.available",0);break;}if(!available)break;
  if(!InternetReadFile(r,data,available>4096?4096:available,&n)){check("response.read",0);break;}if(!n)break;total+=n;if(total>8*1024*1024){check("response.limit",0);break;}}
 line("BODY_BYTES=");hex(total);line("\r\n");check("response.body",total!=0);
 check("response.eof",InternetReadFile(r,data,4096,&n)&&n==0);
cleanup:
 if(s){check("session.close_children",InternetCloseHandle(s));if(r)check("request.invalid_after_parent_close",!InternetCloseHandle(r)&&GetLastError()==6);}
}
__attribute__((section(".text.entry"))) U32 entry(void){U32 dll,h,n,i,session,connection,request,good=1;
 line("PocketHLE ARM NETTEST v2\r\n");dll=LoadLibraryW(L"wininet.dll");check("wininet.load",dll!=0);if(!dll)goto finish;
 BIND(InternetOpenW);BIND(InternetConnectW);BIND(HttpOpenRequestW);BIND(HttpAddRequestHeadersW);BIND(HttpSendRequestW);BIND(HttpQueryInfoW);BIND(InternetQueryDataAvailable);BIND(InternetReadFile);BIND(InternetCloseHandle);
 run(0,0);run(1,0);
 /* Pure handle churn: do not send forty extra requests to a public server. */
 for(i=0;i<40;i++) {
  session=InternetOpenW(L"PocketHLE lifecycle",0,0,0,0);if(!session){good=0;break;}
  connection=InternetConnectW(session,L"example.com",443,0,0,3,0,0);
  request=connection?HttpOpenRequestW(connection,L"GET",L"/",L"HTTP/1.1",0,0,0x00800000,0):0;
  if(!connection||!request)good=0;
  if(!InternetCloseHandle(session))good=0;
  if(request&&(InternetCloseHandle(request)||GetLastError()!=6))good=0;
  if(connection&&(InternetCloseHandle(connection)||GetLastError()!=6))good=0;
  if(!good)break;
 }
 check("handles.40_cascading_close_reopen",good&&i==40);
#ifdef LOCAL_VALIDATION
 run(0,1);
#endif
finish:
 line(failed?"NETTEST_RESULT FAIL\r\n":"NETTEST_RESULT PASS\r\n");h=CreateFileW(L"\\Flash Disk\\NETTEST.TXT",0x40000000,0,0,2,0,0);if(h!=0xffffffff){WriteFile(h,report,used,&n,0);CloseHandle(h);}
 MessageBoxW(0,failed?L"Network test failed: see NETTEST.TXT in Flash Disk (DNS, firewall, proxy, certificates).":L"HTTP and HTTPS test passed. See NETTEST.TXT in Flash Disk.",L"PocketHLE NETTEST",0);return failed?1:0;
}
