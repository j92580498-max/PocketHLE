#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
void __aeabi_memclr4(void*p,U32 n){volatile unsigned char*b=p;while(n--)*b++=0;}
static char report[4096];static U32 used,failed;
static void line(const char*s){while(*s)report[used++]=*s++;}
static void hex(U32 n){U32 i;line("0x");for(i=0;i<8;i++)report[used++]="0123456789abcdef"[(n>>(28-4*i))&15];}
static void check(const char*s,U32 ok){line(ok?"PASS ":"FAIL ");line(s);if(!ok){failed++;line(" error=");hex(GetLastError());}line("\r\n");}
__attribute__((section(".text.entry"))) U32 entry(void){
 U32 h,n,count=0,i,settings[7]={0,1,0,0,0,0,0},query[7]={0};
 line("PocketHLE ARM VIBTEST v2\r\n");
 h=CreateFileW(L"VIB1:",0xc0000000,0,0,3,0,0);check("device.open",h!=0xffffffff);if(h==0xffffffff)goto finish;
 check("device.count",DeviceIoControl(h,0,0,0,&count,4,&n,0)&&count==1&&n==4);
 check("device.continuous",DeviceIoControl(h,2,settings,28,0,0,0,0));
 check("device.close_preserves_vibration",CloseHandle(h));h=0xffffffff;Sleep(500);
 check("nled.current",NLedGetDeviceInfo(2,query)&&query[1]==1);
 settings[1]=0;check("nled.stop",NLedSetDevice(2,settings));Sleep(250);
 settings[1]=2;settings[2]=300000;settings[3]=150000;settings[4]=150000;
 check("nled.pulses",NLedSetDevice(2,settings));Sleep(1800);settings[1]=0;check("nled.stop_pulses",NLedSetDevice(2,settings));Sleep(300);
 settings[1]=2;settings[2]=200000;settings[3]=100000;settings[4]=100000;settings[5]=2;settings[6]=2;
 check("nled.meta_double_pulse",NLedSetDevice(2,settings));Sleep(2000);settings[1]=0;check("nled.stop_meta",NLedSetDevice(2,settings));
 settings[0]=1;check("nled.reject_invalid_motor",!NLedSetDevice(2,settings)&&GetLastError()==87);settings[0]=0;
 check("nled.reject_null",!NLedGetDeviceInfo(0,0)&&GetLastError()==87);
 for(i=0;i<64;i++){settings[1]=1;if(!NLedSetDevice(2,settings))break;settings[1]=0;if(!NLedSetDevice(2,settings))break;}
 check("motor.release_reopen_64",i==64);
finish:
 settings[0]=0;settings[1]=0;NLedSetDevice(2,settings);if(h!=0xffffffff)CloseHandle(h);
 line(failed?"VIBTEST_RESULT FAIL\r\n":"VIBTEST_RESULT PASS\r\n");
 h=CreateFileW(L"\\Flash Disk\\VIBTEST.TXT",0x40000000,0,0,2,0,0);if(h!=0xffffffff){WriteFile(h,report,used,&n,0);CloseHandle(h);}
 MessageBoxW(0,failed?L"Vibration API test failed. See VIBTEST.TXT in Flash Disk.":L"API test passed. On Android: expect one short buzz, regular pulses, then double pulses. See VIBTEST.TXT.",L"PocketHLE VIBTEST",0);return failed?1:0;
}
