// Compatibility shim for .NET Compact Framework applications that are run
// through the managed fallback (see frontends/pocket-cli/src/managed.rs).
//
// The Compact Framework ships Microsoft.WindowsCE.Forms.dll with the strong
// name token 969db8053d3322ac; Mono does not provide it, so any CF title
// that touches SystemSettings or ScreenOrientation dies with a
// TypeLoadException before its first window appears. This shim provides the
// subset of the API surface the hosted games actually use, on top of the
// host windowing system (which has no concept of device rotation).
//
// Rebuild with: mcs -target:library -out:Microsoft.WindowsCE.Forms.dll Microsoft.WindowsCE.Forms.cs

using System;

[assembly: System.Reflection.AssemblyVersion("3.5.0.0")]

namespace Microsoft.WindowsCE.Forms
{
    /// <summary>CF ScreenOrientation values. The host display is never
    /// rotated, so Angle0 is the only state ever reported.</summary>
    public enum ScreenOrientation
    {
        Angle0 = 1,
        Angle90 = 2,
        Angle180 = 3,
        Angle270 = 4,
    }

    /// <summary>Stub of the CF SystemSettings class. Only the
    /// ScreenOrientation property is emulated; reads report Angle0 and
    /// writes are accepted and ignored.</summary>
    public static class SystemSettings
    {
        public static ScreenOrientation ScreenOrientation
        {
            get { return ScreenOrientation.Angle0; }
            set { }
        }
    }
}
