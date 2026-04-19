"""Invoice-rendering service startup is slow.

render_invoice() itself is trivial; the expensive work hides in
_load_templates() which is called from the constructor each time
Renderer() is instantiated — and the fast-path (fmt()) instantiates
a fresh Renderer per invoice to "stay stateless". A cProfile run
shows most of tottime in _load_templates / re.compile, not in the
body of fmt() where the team was looking."""

import re
import time


class Renderer:
    def __init__(self):
        self._patterns = self._load_templates()

    def _load_templates(self):
        # ~1000 regexes — happens to be expensive. In the real code
        # this is "load all format strings and compile escape rules".
        compiled = {}
        for i in range(1000):
            compiled[i] = re.compile(r"(\w+)=(\d+\.?\d*)#%d" % i)
        return compiled

    def fmt(self, invoice_id: int, amount: float) -> str:
        return f"invoice={invoice_id}#amount={amount:.2f}"


def render_invoice(invoice_id: int, amount: float) -> str:
    r = Renderer()
    return r.fmt(invoice_id, amount)


def main():
    # 200 "requests" — small workload; but we're paying _load_templates
    # on every one.
    t0 = time.time()
    total = 0
    for i in range(200):
        s = render_invoice(i, 12.34 + i)
        total += len(s)
    print(f"rendered {total} chars in {time.time() - t0:.2f}s")


if __name__ == "__main__":
    main()
