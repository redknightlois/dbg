// A request-dispatcher that builds a route key on every call by
// joining method + path + query. Innocuous-looking string ops —
// but in a hot loop they produce a lot of short-lived garbage and
// GC pauses dominate at high RPS. `dotnet-trace collect` exposes
// the gen0 allocation rate; flamegraph traces hot stacks to
// `BuildKey`.

using System.Diagnostics;

static class Program {
    static string BuildKey(string method, string path, int tenant, long reqId) {
        // Each of these creates a new string. In aggregate: ~6
        // allocations per request, and the app processes millions.
        var a = method.ToUpper();
        var b = path.Replace("/", "_");
        var c = tenant.ToString();
        var d = reqId.ToString();
        return string.Join(":", a, b, c, d);
    }

    static long Dispatch(string method, string path, int tenant, int n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            var k = BuildKey(method, path, tenant, i);
            acc += k.Length;
        }
        return acc;
    }

    static void Main() {
        var sw = Stopwatch.StartNew();
        var total = Dispatch("get", "/api/v1/orders/list", 42, 2_000_000);
        sw.Stop();
        System.Console.WriteLine($"total={total} elapsed={sw.ElapsedMilliseconds}ms");
    }
}
