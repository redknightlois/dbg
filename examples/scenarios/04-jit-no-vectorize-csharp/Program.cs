// Two near-identical hot loops summing a float array. `SumFast`
// is what we want the JIT to vectorize. `SumSlow` is what we
// noticed in production — it benchmarks ~5x slower than expected
// and we suspect the JIT is bailing out of auto-vectorization.
//
// The two functions differ by *one* line. Find it via jitdasm.

using System.Diagnostics;

namespace Broken;

public static class Program
{
    public static float SumFast(float[] xs)
    {
        float acc = 0f;
        for (int i = 0; i < xs.Length; i++)
        {
            acc += xs[i];
        }
        return acc;
    }

    public static float SumSlow(float[] xs)
    {
        float acc = 0f;
        for (int i = 0; i <= xs.Length - 1; i++)   // ← subtle
        {
            if (i >= xs.Length) break;             // ← redundant guard the JIT chokes on
            acc += xs[i];
        }
        return acc;
    }

    public static void Main()
    {
        const int N = 1 << 20;
        var xs = new float[N];
        for (int i = 0; i < N; i++) xs[i] = i * 0.0001f;

        // Warm up tiered JIT.
        for (int t = 0; t < 1000; t++) { SumFast(xs); SumSlow(xs); }

        var sw = Stopwatch.StartNew();
        float a = 0f;
        for (int t = 0; t < 200; t++) a += SumFast(xs);
        var fastMs = sw.Elapsed.TotalMilliseconds;

        sw.Restart();
        float b = 0f;
        for (int t = 0; t < 200; t++) b += SumSlow(xs);
        var slowMs = sw.Elapsed.TotalMilliseconds;

        System.Console.WriteLine($"SumFast: {fastMs:F1} ms  (sum={a})");
        System.Console.WriteLine($"SumSlow: {slowMs:F1} ms  (sum={b})");
        System.Console.WriteLine($"ratio  : {slowMs / fastMs:F2}x");
    }
}
