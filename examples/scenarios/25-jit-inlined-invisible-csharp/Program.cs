// A fixed-capacity open-addressing hash map. `TryGetValue` is the
// hottest method in production profiles — a single call per lookup,
// doing a mask-and-probe. We want to confirm it vectorizes poorly
// (scalar probing is expected) and that the bounds check is elided.
//
// The catch: `TryGetValue` is a *small hot method*. The JIT inlines
// it at every call site, so a naive `dbg disasm FlatLongIntMap:TryGetValue`
// returns "no methods found" — even though the code absolutely runs.
//
// This scenario exercises the inlined-fallback path: dbg's jitdasm
// REPL should notice that `TryGetValue` has no standalone listing,
// look up its callers in the capture's call graph, and display *those*
// methods' disassembly so the inlined body can be inspected where it
// actually lives (embedded inside the parent).
//
// The target inspection: find the mask-and-probe sequence of
// `TryGetValue` inside `LookupBatch`'s body, and confirm there is no
// RNGCHKFAIL on the slot array access.
//
// Run:
//   dotnet build -c Release
//   dbg start jitdasm Broken.csproj --args 'FlatLongIntMap:TryGetValue' --capture-duration 15s

using System.Diagnostics;
using System.Runtime.CompilerServices;

namespace Broken;

public sealed class FlatLongIntMap
{
    private readonly long[] _keys;
    private readonly int[] _values;
    private readonly int _mask;

    public FlatLongIntMap(int capacityPow2)
    {
        _keys = new long[capacityPow2];
        _values = new int[capacityPow2];
        _mask = capacityPow2 - 1;
        for (int i = 0; i < capacityPow2; i++) _keys[i] = -1;
    }

    // Private, tiny, aggressive-inline — the JIT has no reason to emit
    // a standalone body for this helper. It is the "invisible" target
    // the scenario is built around: every caller gets the three
    // instructions inlined in place; `capture.asm` never contains
    // `; Assembly listing for method …:Slot` on its own.
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private int Slot(long key) => (int)(key & _mask);

    public void Put(long key, int value)
    {
        int i = Slot(key);
        while (_keys[i] != -1 && _keys[i] != key)
            i = (i + 1) & _mask;
        _keys[i] = key;
        _values[i] = value;
    }

    public bool TryGetValue(long key, out int value)
    {
        int i = Slot(key);
        while (true)
        {
            long k = _keys[i];
            if (k == key) { value = _values[i]; return true; }
            if (k == -1) { value = 0; return false; }
            i = (i + 1) & _mask;
        }
    }
}

public static class Program
{
    // A caller that will show up standalone in the capture. The
    // inlined body of `TryGetValue` lives embedded in this method's
    // disassembly.
    public static long LookupBatch(FlatLongIntMap map, long[] keys)
    {
        long sum = 0;
        for (int i = 0; i < keys.Length; i++)
        {
            if (map.TryGetValue(keys[i], out int v)) sum += v;
        }
        return sum;
    }

    // Second caller so the fallback has more than one parent to show —
    // exercises the "rank callers by code size, cap at N" behavior.
    public static int LookupOne(FlatLongIntMap map, long key)
    {
        return map.TryGetValue(key, out int v) ? v : -1;
    }

    public static void Main()
    {
        const int Capacity = 1 << 16;
        var map = new FlatLongIntMap(Capacity);

        var rng = new System.Random(42);
        var keys = new long[1 << 14];
        for (int i = 0; i < keys.Length; i++)
        {
            long k = rng.NextInt64();
            keys[i] = k;
            map.Put(k, i);
        }

        // Warm up tiered JIT so the final capture reflects optimized code.
        for (int t = 0; t < 1000; t++)
        {
            LookupBatch(map, keys);
            LookupOne(map, keys[t & (keys.Length - 1)]);
        }

        var sw = Stopwatch.StartNew();
        long total = 0;
        for (int t = 0; t < 200; t++) total += LookupBatch(map, keys);
        sw.Stop();

        System.Console.WriteLine($"LookupBatch: {sw.Elapsed.TotalMilliseconds:F1} ms  (sum={total})");
    }
}
