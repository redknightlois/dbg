
; Assembly listing for method MyNamespace.SimdOps:DotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float (FullOpts)
; Emitting BLENDED_CODE for generic X64 + VEX on Unix
; FullOpts code
; optimized code

G_M000_IG01:
       push     rbp
       mov      rbp, rsp

G_M000_IG02:
       vxorps   ymm0, ymm0, ymm0
       vmovups  ymm1, ymmword ptr [r10]
       vmulps   ymm1, ymm1, ymmword ptr [r8]
       vaddps   ymm0, ymm1, ymm0
       vdpps    ymm0, ymm0, ymmword ptr [reloc @RWD00], -1

G_M000_IG03:
       vmovss   xmm1, dword ptr [rdi+4*rcx]
       vmulss   xmm1, xmm1, dword ptr [rdx+4*rcx]
       vaddss   xmm0, xmm1, xmm0

G_M000_IG04:
       call     CORINFO_HELP_RNGCHKFAIL
       int3

; Total bytes of code 250
; Assembly listing for method MyNamespace.SimdOps:ScalarDotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float (FullOpts)
; FullOpts code
; optimized code

G_M000_IG01:
       push     rbp
       mov      rbp, rsp

G_M000_IG02:
       vxorps   xmm0, xmm0, xmm0
       vmovss   xmm1, dword ptr [rdi+rax]
       vmulss   xmm1, xmm1, dword ptr [rdx+rax]
       vaddss   xmm0, xmm1, xmm0

G_M000_IG03:
       call     CORINFO_HELP_RNGCHKFAIL
       int3

; Total bytes of code 96
; Assembly listing for method MyNamespace.MathUtils:Normalize(float[]):float[] (FullOpts)
; FullOpts code
; optimized code

G_M000_IG01:
       push     rbp

G_M000_IG02:
       call     [MyNamespace.SimdOps:DotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float]
       vmovss   xmm0, dword ptr [rdi+rax]
       vmulss   xmm0, xmm0, xmm1
       mov      qword ptr [rsp+0x10], rax

G_M000_IG03:
       call     [MyNamespace.MathUtils:Length(float[]):float]
       pop      rbp
       ret

; Total bytes of code 64
; Assembly listing for method MyNamespace.Pipeline:Run(float[],float[]):float (FullOpts)
; FullOpts code
; optimized code

G_M000_IG01:
       push     rbp
       mov      rbp, rsp

G_M000_IG02:
       call     [MyNamespace.MathUtils:Normalize(float[]):float[]]
       call     [MyNamespace.SimdOps:DotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float]

G_M000_IG03:
       call     [MyNamespace.SimdOps:ScalarDotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float]

G_M000_IG04:
       pop      rbp
       ret

; Total bytes of code 48
