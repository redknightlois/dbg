// Inventory deduplication. We read SKUs from two upstream feeds
// and merge them into a HashMap<Sku, Integer> counting occurrences.
// After merge, the count for SKU "A-1001" should be 3 — it appears
// once in feed1 and twice in feed2. We get 1.
//
// Build: javac -g Broken.java
// Run:   java Broken

import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class Broken {

    static final class Sku {
        final String prefix;
        final int number;
        Sku(String prefix, int number) {
            this.prefix = prefix;
            this.number = number;
        }
        @Override public boolean equals(Object o) {
            if (!(o instanceof Sku)) return false;
            Sku s = (Sku) o;
            return this.prefix.equals(s.prefix) && this.number == s.number;
        }
        @Override public int hashCode() {
            return prefix.hashCode() * 31 + number;
        }
        @Override public String toString() {
            return prefix + "-" + number;
        }
    }

    static Map<Sku, Integer> merge(List<Sku> a, List<Sku> b) {
        Map<Sku, Integer> counts = new HashMap<>();
        for (Sku s : a) counts.merge(s, 1, Integer::sum);
        for (Sku s : b) counts.merge(s, 1, Integer::sum);
        return counts;
    }

    public static void main(String[] args) {
        List<Sku> feed1 = List.of(new Sku("A", 1001));
        List<Sku> feed2 = List.of(new Sku("A", 1001), new Sku("A", 1001));
        Map<Sku, Integer> counts = merge(feed1, feed2);
        System.out.println("counts:");
        for (Map.Entry<Sku, Integer> e : counts.entrySet()) {
            System.out.println("  " + e.getKey() + " -> " + e.getValue());
        }
        Sku probe = new Sku("A", 1001);
        Integer got = counts.get(probe);
        if (got == null || got != 3) {
            System.err.println("BUG: expected 3 for A-1001, got " + got);
            System.exit(1);
        }
        System.out.println("OK");
    }
}
