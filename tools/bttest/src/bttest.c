#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
#define BAD 0xffffffffu
void __aeabi_memclr4(void*p,U32 n){volatile unsigned char*b=p;while(n--)*b++=0;}
static char report[32768];static U32 used,failures,truncated;
static W16 display[4096];static U32 displayed;
static void byte(char c){if(used<sizeof(report)-1)report[used++]=c;else truncated=1;}
static void line(const char*s){while(*s)byte(*s++);}
static void wide(const W16*s){while(*s&&displayed<4095)display[displayed++]=*s++;}
static void decimal(U32 v){char a[10];U32 i=0;do{a[i++]=(char)('0'+v%10);v/=10;}while(v);while(i)byte(a[--i]);}
static void hex(U32 v){U32 i;line("0x");for(i=0;i<8;i++)byte("0123456789abcdef"[(v>>(28-i*4))&15]);}
static void stage(const char*s,U32 ok,U32 error){line(ok?"PASS ":"FAIL ");line(s);if(!ok){failures++;line(" error=");decimal(error);line(" (");hex(error);line(")");}line("\r\n");}
static U32 save(void){U32 n=0,h=CreateFileW(L"\\Flash Disk\\BTTEST.TXT",0x40000000,0,0,2,0,0),ok;if(h==BAD)return 0;ok=WriteFile(h,report,used,&n,0)&&n==used;CloseHandle(h);return ok;}
static void utf8(const W16*s){
 U32 n=0;while(*s&&n++<128){U32 c=*s++;
  if(c>=0xd800&&c<=0xdbff&&*s>=0xdc00&&*s<=0xdfff)c=0x10000+((c-0xd800)<<10)+(*s++-0xdc00);
  else if(c>=0xd800&&c<=0xdfff)c=0xfffd;
  if(c<0x80)byte((char)c);
  else if(c<0x800){byte(0xc0|(c>>6));byte(0x80|(c&63));}
  else if(c<0x10000){byte(0xe0|(c>>12));byte(0x80|((c>>6)&63));byte(0x80|(c&63));}
  else{byte(0xf0|(c>>18));byte(0x80|((c>>12)&63));byte(0x80|((c>>6)&63));byte(0x80|(c&63));}
 }if(*s)line("...");
}
static U32 in_buffer(U32 ptr,U32 size,U32 start,U32 length){return ptr>=start&&size<=length&&ptr-start<=length-size;}
static U32 device(void*p,U32 length,U32 index){
 U32*words=p;U32 begin=(U32)p;U32 cs,remote,name,i,n;unsigned char*addr;W16 mac[18];
 if(length<124||words[0]!=60||words[5]!=16||words[11]<1)return 0;
 cs=words[12];name=words[1];if(!in_buffer(cs,24,begin,length))return 0;
 remote=((U32*)cs)[2];if(((U32*)cs)[3]<40||!in_buffer(remote,40,begin,length))return 0;
 if(*(W16*)remote!=32)return 0;
 if(!in_buffer(name,2,begin,length))return 0;
 for(n=0;n<512;n++){if(!in_buffer(name+n*2,2,begin,length))return 0;if(!((W16*)name)[n])break;}
 if(n==512)return 0;
 addr=(unsigned char*)remote+8;
 for(i=0;i<6;i++){unsigned char v=addr[5-i];mac[i*3]="0123456789ABCDEF"[v>>4];mac[i*3+1]="0123456789ABCDEF"[v&15];mac[i*3+2]=i==5?0:':';}
 line("DEVICE ");decimal(index);line(" address=");for(i=0;i<17;i++)byte((char)mac[i]);line(" name=");utf8((W16*)name);line("\r\n");
 if(index<=12){wide(mac);wide(L"  ");for(i=0;i<n&&i<64&&displayed<4094;i++)display[displayed++]=((W16*)name)[i];wide(L"\n");}
 return 1;
}
__attribute__((section(".text.entry"))) U32 entry(void){
 U32 msg=0,startup=0,lookup=0,count=0,code,i,good=1,query[15]={0},data[100]={0},length;char hostname[128]={0};void*result=0;
 line("\xef\xbb\xbfPocketHLE ARM Bluetooth Classic inquiry v2\r\n");
 line("Paired/remembered and discovered devices; not a BLE or SDP service scan.\r\n");
 wide(L"Bluetooth Classic devices\n\n");
 msg=RegisterWindowMessageW(L"BT_MSG");stage("sdk.register_bt_message",msg!=0,GetLastError());if(!msg)goto finish;
 SendMessageW(0xffff,msg,1,0);Sleep(1000);
 for(i=0;i<32;i++){code=WSAStartup(0x0202,data);if(code){good=0;break;}if(WSACleanup()!=0){good=0;break;}}
 stage("wsa.32_startup_cleanup_cycles",good&&i==32,code);if(!good)goto finish;
 code=WSAStartup(0x0202,data);stage("wsa.startup",code==0,code);if(code)goto finish;startup=1;
 code=gethostname(hostname,sizeof(hostname));stage("host.name",code==0,code?WSAGetLastError():0);
 if(!code){line("HOST ");line(hostname);line("\r\n");}
 query[0]=60;query[5]=16;
 for(i=0;i<8;i++){lookup=0;code=WSALookupServiceBeginW(query,2,&lookup);if(code||!lookup){good=0;break;}code=WSALookupServiceEnd(lookup);lookup=0;if(code){good=0;break;}}
 stage("inquiry.8_cancel_reopen_cycles",good&&i==8,code?WSAGetLastError():0);
 code=WSALookupServiceBeginW(query,2,&lookup);stage("inquiry.begin",code==0,code?WSAGetLastError():0);if(code){lookup=0;goto finish;}
 line("SCAN_PENDING\r\n");save();
 while(count<64){
  length=0;code=WSALookupServiceNextW(lookup,0x110,&length,0);
  if(code==BAD){U32 e=WSAGetLastError();if(e==10110||e==10102){line("PASS inquiry.end_of_results\r\n");break;}
   if(e!=10014){stage("inquiry.next_size",0,e);goto finish;}}
  if(length<124||length>1048576){stage("inquiry.result_size",0,13);goto finish;}
  result=VirtualAlloc(0,length,0x3000,4);if(!result){stage("inquiry.allocate",0,GetLastError());goto finish;}
  code=WSALookupServiceNextW(lookup,0x110,&length,result);
  if(code){stage("inquiry.next",0,WSAGetLastError());goto finish;}
  if(!device(result,length,count+1)){stage("inquiry.guest_pointers",0,13);goto finish;}
  count++;VirtualFree(result,0,0x8000);result=0;
 }
 if(count==64)line("LIST_LIMIT 64 (remaining results not requested)\r\n");
 line("DEVICE_COUNT ");decimal(count);line("\r\n");
 if(!count){line("NO_DEVICES: scan completed; enable discoverable mode on a nearby Classic device and retry.\r\n");wide(L"No devices found.\n");}
 if(count>12)wide(L"More devices are listed in BTTEST.TXT.\n");
finish:
 if(result)VirtualFree(result,0,0x8000);
 if(lookup){code=WSALookupServiceEnd(lookup);stage("inquiry.close",code==0,code?WSAGetLastError():0);}
 if(startup){code=WSACleanup();stage("wsa.cleanup",code==0,code?WSAGetLastError():0);}
 if(truncated){failures++;line("REPORT_TRUNCATED\r\n");}
 line(failures?"BTTEST_RESULT FAIL\r\n":"BTTEST_RESULT PASS\r\n");
 if(!save()){failures++;wide(L"Could not write BTTEST.TXT.\n");}
 wide(failures?L"\nScan failed: see BTTEST.TXT for error details.":L"\nScan complete: see BTTEST.TXT in Flash Disk.");
 display[displayed]=0;MessageBoxW(0,display,L"PocketHLE BTTEST",0);
 return failures?1:0;
}
