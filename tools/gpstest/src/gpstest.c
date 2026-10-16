#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
void __aeabi_memclr4(void*p,U32 n){volatile unsigned char*b=p;while(n--)*b++=0;}
static char report[4096];static U32 used,failed;
static void line(const char*s){while(*s)report[used++]=*s++;}
static void hex(U32 n){U32 i;line("0x");for(i=0;i<8;i++)report[used++]="0123456789abcdef"[(n>>(28-4*i))&15];}
static void check(const char*s,U32 ok){line(ok?"PASS ":"FAIL ");line(s);if(!ok){failed++;line(" error=");hex(GetLastError());}line("\r\n");}
static U32 word(const unsigned char*p,U32 o){return p[o]|p[o+1]<<8|p[o+2]<<16|p[o+3]<<24;}
static U32 save(const W16*path,const void*p,U32 size){U32 n=0,h=CreateFileW(path,0x40000000,0,0,2,0,0),ok;if(h==0xffffffff)return 0;ok=WriteFile(h,p,size,&n,0)&&n==size;CloseHandle(h);return ok;}
__attribute__((section(".text.entry"))) U32 entry(void){
 unsigned char p[180]={0};U32 h,other,n,t,i,fix=0;
 line("PocketHLE ARM GPS1 test v2\r\n");
 h=CreateFileW(L"GPS1:",0x80000000,0,0,3,0,0);check("gps.open",h!=0xffffffff);if(h==0xffffffff)goto finish;
 other=CreateFileW(L"GPS1:",0x80000000,0,0,3,0,0);check("gps.exclusive_share",other==0xffffffff&&GetLastError()==32);if(other!=0xffffffff)CloseHandle(other);
 check("gps.read_packed_180",ReadFile(h,p,180,&n,0)&&n==180);
 check("gps.reject_short",!ReadFile(h,p,179,&n,0)&&n==0&&GetLastError()==87);
 check("gps.reject_null",!ReadFile(h,0,180,&n,0)&&GetLastError()==87);
 check("gps.reject_write_on_read_handle",!WriteFile(h,p,21,&n,0)&&GetLastError()==5);
 t=GetTickCount();
 do {if(!ReadFile(h,p,180,&n,0)){check("gps.poll_native_position",0);break;}fix=word(p,15)!=0;if(fix)break;Sleep(100);}while(GetTickCount()-t<30000);
 line(fix?"GPS_POSITION AVAILABLE\r\n":"GPS_POSITION NO_FIX (permission/service/provider may be unavailable)\r\n");
 line("latitude_e7=");hex(word(p,21));line(" longitude_e7=");hex(word(p,25));line(" horizontal_error_cm=");hex(word(p,37));line("\r\n");
 line(fix&&word(p,37)<10000?"COLORS_POSITION_ELIGIBLE YES\r\n":"COLORS_POSITION_ELIGIBLE NO\r\n");
 check("gps.save_snapshot",save(L"\\Flash Disk\\GPSTEST.BIN",p,180));
 check("gps.close",CloseHandle(h));h=0xffffffff;
 for(i=0;i<32;i++){h=CreateFileW(L"GPS1:",0x80000000,0,0,3,0,0);if(h==0xffffffff||!ReadFile(h,p,180,&n,0)||n!=180||!CloseHandle(h)){check("gps.read_reopen_close_32",0);break;}h=0xffffffff;}
 if(i==32)check("gps.read_reopen_close_32",1);
finish:
 if(h!=0xffffffff)CloseHandle(h);
 line(failed?"GPSTEST_RESULT FAIL\r\n":"GPSTEST_RESULT PASS\r\n");save(L"\\Flash Disk\\GPSTEST.TXT",report,used);
 MessageBoxW(0,failed?L"GPS test failed. See GPSTEST.TXT in Flash Disk. Enable GPS / host location and check OS permissions.":fix?L"GPS test passed, native position received. See GPSTEST.TXT and GPSTEST.BIN in Flash Disk.":L"GPS API test passed, but no location fix within 30 seconds. See GPSTEST.TXT; check OS location settings and retry outdoors on Android.",L"PocketHLE GPSTEST",0);
 return failed?1:0;
}
