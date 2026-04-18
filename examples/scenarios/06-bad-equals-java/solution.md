`Sku` overrides `equals` but not `hashCode`. The default `Object.hashCode` returns identity-based hashes, so two distinct `Sku` instances with identical fields land in different buckets — `HashMap` never even calls `equals` to compare them. Each insert produces a "new" key, and the freshly-constructed probe finds no match.

Fix: add a matching `hashCode`:

```java
@Override public int hashCode() {
    return java.util.Objects.hash(prefix, number);
}
```

The contract is: equal objects must have equal hash codes. Violating it silently breaks every hash-based collection.
