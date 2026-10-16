/* Freestanding ARM timing contracts. Upper latency is reported, never hidden. */
#include "imports.h"
typedef unsigned int U32;typedef unsigned short W16;
static char report[12000];static U32 used,checks,failed,io_failed;
static U32 event_signal;static volatile U32 timer_count,timer_bad,timer_id,wave_count,wave_bad;
void __aeabi_memclr4(void*p,U32 n){volatile unsigned char*b=p;while(n--)*b++=0;}
static void line(const char*s){while(*s&&used<sizeof(report)-1)report[used++]=*s++;if(*s)io_failed=1;}
static void hex(U32 n){U32 i;line("0x");for(i=0;i<8;i++){char b[2]={"0123456789abcdef"[(n>>(28-4*i))&15],0};line(b);}}
static void check(const char*s,U32 ok){checks++;if(!ok)failed++;line(ok?"PASS ":"FAIL ");line(s);line("\r\n");}
static void measure(const char*s,U32 elapsed){line("MEASURE ");line(s);line(" milliseconds=");hex(elapsed);line("\r\n");if(elapsed>2000)line("WARN host_latency_over_2_seconds\r\n");}
static U32 elapsed(U32 start){return GetTickCount()-start;}
static U32 signal_worker(void*unused){(void)unused;Sleep(30);SetEvent(event_signal);return 77;}
static void timer_proc(U32 hwnd,U32 msg,U32 id,U32 time){(void)hwnd;(void)time;if(msg!=0x113||id!=timer_id)timer_bad=1;timer_count++;}
static void wave_proc(U32 h,U32 msg,U32 instance,U32 p1,U32 p2){(void)h;(void)p1;(void)p2;if(msg==0x3bd){if(instance!=0x12345678)wave_bad=1;Sleep(1);wave_count++;}}
static void pump(U32 duration){U32 start=GetTickCount(),msg[7];while(elapsed(start)<duration){if(PeekMessageW(msg,0,0,0,1))DispatchMessageW(msg);Sleep(1);}}
static void sleeps_and_waits(void){
 U32 i,start,e[2],rc,id=0,h=0,code=0;U32 all[2];
 for(i=0;i<8;i++){start=GetTickCount();Sleep(20);rc=elapsed(start);measure("sleep20",rc);check("sleep.positive_not_early",rc>=19);}
 e[0]=CreateEventW(0,1,0,0);e[1]=CreateEventW(0,1,0,0);check("wait.events_open",e[0]&&e[1]);
 if(e[0]&&e[1]){
  start=GetTickCount();rc=WaitForSingleObject(e[0],20);check("wait.single_timeout",rc==258);check("wait.single_not_early",elapsed(start)>=19);measure("single_wait20",elapsed(start));
  all[0]=e[0];all[1]=e[1];SetEvent(e[0]);start=GetTickCount();rc=WaitForMultipleObjects(2,all,1,20);
  check("wait.all_timeout_with_one_signaled",rc==258&&elapsed(start)>=19);check("wait.timeout_preserves_manual_event",WaitForSingleObject(e[0],0)==0);
  SetEvent(e[1]);check("wait.all_success",WaitForMultipleObjects(2,all,1,0)==0);
  event_signal=CreateEventW(0,0,0,0);h=CreateThread(0,0x40000,signal_worker,0,0,&id);check("wait.worker_started",event_signal&&h);
  if(h&&event_signal){start=GetTickCount();check("wait.worker_signal",WaitForSingleObject(event_signal,3000)==0);measure("worker_signal",elapsed(start));
   rc=WaitForSingleObject(h,3000);check("wait.worker_exit",rc==0&&GetExitCodeThread(h,&code)&&code==77);if(rc!=0)TerminateThread(h,1);}
 }
 if(h)CloseHandle(h);if(event_signal)CloseHandle(event_signal);if(e[0])CloseHandle(e[0]);if(e[1])CloseHandle(e[1]);
}
static void timers(void){
 U32 previous;timer_id=SetTimer(0,71,20,timer_proc);check("timer.start",timer_id!=0);if(!timer_id)return;
 pump(120);check("timer.callback_delivered",timer_count>=2&&!timer_bad);check("timer.cancel",KillTimer(0,timer_id));
 previous=timer_count;pump(80);check("timer.no_callback_after_cancel",timer_count==previous);
 timer_id=SetTimer(0,72,20,timer_proc);previous=timer_count;pump(70);check("timer.restart",timer_id!=0&&timer_count>previous&&!timer_bad);if(timer_id)KillTimer(0,timer_id);
}
static void waveout(void){
 /* PCM mono, 8 kHz, 16-bit, 100 ms. Zero samples deliberately avoid loud audio. */
 unsigned char format[18]={1,0,1,0,0x40,0x1f,0,0,0x80,0x3e,0,0,2,0,16,0,0,0};
 static short samples[800];U32 hdr[8]={0},h=0,e=0,start,rc,i,good=1;
 hdr[0]=(U32)samples;hdr[1]=sizeof(samples);
 e=CreateEventW(0,0,0,0);rc=waveOutOpen(&h,0xffffffff,format,e,0,0x50000);check("audio.event_open",rc==0&&e&&h);
 if(rc==0&&h){ResetEvent(e);check("audio.event_prepare",waveOutPrepareHeader(h,hdr,32)==0);start=GetTickCount();check("audio.event_write",waveOutWrite(h,hdr,32)==0);
  check("audio.event_completion",WaitForSingleObject(e,4000)==0&&(hdr[4]&1));measure("audio100",elapsed(start));
  check("audio.event_not_early",elapsed(start)>=90);waveOutReset(h);waveOutUnprepareHeader(h,hdr,32);check("audio.event_close",waveOutClose(h)==0);h=0;}
 if(e)CloseHandle(e);
 hdr[4]=0;hdr[5]=0;rc=waveOutOpen(&h,0xffffffff,format,(U32)wave_proc,0x12345678,0x30000);check("audio.function_open",rc==0&&h);
 if(rc==0&&h){check("audio.function_prepare",waveOutPrepareHeader(h,hdr,32)==0);check("audio.function_write",waveOutWrite(h,hdr,32)==0);start=GetTickCount();while(!wave_count&&elapsed(start)<4000)Sleep(5);
  check("audio.function_nested_sleep_completion",wave_count==1&&!wave_bad&&(hdr[4]&1));waveOutReset(h);waveOutUnprepareHeader(h,hdr,32);check("audio.function_close",waveOutClose(h)==0);h=0;}
 for(i=0;i<40;i++){rc=waveOutOpen(&h,0xffffffff,format,0,0,0);if(rc||!h){good=0;break;}if(waveOutClose(h)!=0){good=0;break;}h=0;}
 check("audio.40_open_close_cycles",good&&i==40);if(h){waveOutReset(h);waveOutClose(h);}
}
static void egl(void){
 U32 dll=LoadLibraryW(L"libgles_cm.dll"),dpy=0,cfg=0,num=0,surface=0,context=0,major=0,minor=0,start,i,good=1;
 U32 (*getdisplay)(U32)=0,(*initialize)(U32,U32*,U32*)=0,(*choose)(U32,const U32*,U32*,U32,U32*)=0;
 U32 (*window)(U32,U32,U32,const U32*)=0,(*create)(U32,U32,U32,const U32*)=0,(*current)(U32,U32,U32,U32)=0;
 U32 (*swap)(U32,U32)=0,(*destroySurface)(U32,U32)=0,(*destroyContext)(U32,U32)=0,(*terminate)(U32)=0;
 U32 attr[1]={0x3038};
 check("egl.library",dll!=0);if(!dll)return;
#define BIND(var,name) var=(void*)GetProcAddressA(dll,name);if(!var){check("egl.required_export",0);goto done;}
 BIND(getdisplay,"eglGetDisplay");BIND(initialize,"eglInitialize");BIND(choose,"eglChooseConfig");BIND(window,"eglCreateWindowSurface");BIND(create,"eglCreateContext");BIND(current,"eglMakeCurrent");BIND(swap,"eglSwapBuffers");BIND(destroySurface,"eglDestroySurface");BIND(destroyContext,"eglDestroyContext");BIND(terminate,"eglTerminate");
 dpy=getdisplay(0);check("egl.initialize",dpy&&initialize(dpy,&major,&minor));
 check("egl.config",choose(dpy,attr,&cfg,1,&num)&&cfg&&num==1);
 surface=window(dpy,cfg,0,attr);context=create(dpy,cfg,0,attr);check("egl.make_current",surface&&context&&current(dpy,surface,surface,context));
 start=GetTickCount();for(i=0;i<12;i++)if(!swap(dpy,surface)){good=0;break;}
 check("egl.identical_window_frames_paced",good&&i==12&&elapsed(start)>=150);measure("egl12_swaps",elapsed(start));
 Sleep(50);check("egl.late_resume_swap",swap(dpy,surface));start=GetTickCount();check("egl.next_swap_does_not_catch_up",swap(dpy,surface)&&elapsed(start)>=14);
done:
 if(current&&dpy)current(dpy,0,0,0);if(destroySurface&&surface)check("egl.destroy_surface",destroySurface(dpy,surface));if(destroyContext&&context)check("egl.destroy_context",destroyContext(dpy,context));if(terminate&&dpy)check("egl.terminate",terminate(dpy));FreeLibrary(dll);
}
__attribute__((section(".text.entry"))) U32 entry(void){U32 h,n=0;
 line("PocketHLE ARM TIMETEST v1\r\n");sleeps_and_waits();timers();waveout();egl();
 line(failed||io_failed?"TIMETEST_RESULT FAIL checks=":"TIMETEST_RESULT PASS checks=");hex(checks);line(" failures=");hex(failed);line("\r\n");
 h=CreateFileW(L"\\Flash Disk\\TIMETEST.TXT",0x40000000,0,0,2,0,0);if(h==0xffffffff)io_failed=1;else{if(!WriteFile(h,report,used,&n,0)||n!=used)io_failed=1;CloseHandle(h);}
 MessageBoxW(0,failed||io_failed?L"FAIL: see TIMETEST.TXT in Flash Disk.":L"PASS: timing contracts. See TIMETEST.TXT for measured latency.",L"PocketHLE TIMETEST",0);
 return failed||io_failed?1:0;
}
