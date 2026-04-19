The leftover TODO was the clue:

```python
distance_nm = distance_miles  # ← someone left a TODO here once
```

The inputs are statute miles, the burn table is per nautical mile. 1 statute mile = 0.8689762 nautical miles (≈ 1/1.151). Fix:

```python
distance_nm = distance_miles * 0.8689762
```

Now Boston→Denver returns ~3500 L instead of ~4025 L.
