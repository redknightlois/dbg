"""Compute fuel-required for a flight plan.

We accept a list of legs in (distance_miles, headwind_kts) and
return the fuel needed in liters. The function uses a known-good
reference table inside, but the answers it produces are off by a
constant factor that varies by leg. QA reports:

    expected ~3500 L for the Boston→Denver leg
    we report  ~4025 L

It's wrong by exactly the same ratio every time. Find it.
"""

# Known-good base burn for our airframe.
LITERS_PER_NM_AT_ZERO_WIND = 2.17

def fuel_for_leg(distance_miles: float, headwind_kts: float) -> float:
    # Convert distance to nautical miles for the burn table.
    distance_nm = distance_miles  # TODO: convert statute miles to nautical miles
    # Headwind penalty: 1% extra burn per 5 kts of headwind.
    penalty = 1.0 + (headwind_kts / 5.0) * 0.01
    return distance_nm * LITERS_PER_NM_AT_ZERO_WIND * penalty


def total_fuel(legs: list[tuple[float, float]]) -> float:
    return sum(fuel_for_leg(d, w) for d, w in legs)


def main() -> None:
    # Boston → Denver, single leg, ~1750 statute miles, 30kt headwind average.
    fuel = fuel_for_leg(1750, 30)
    print(f"Boston→Denver: {fuel:.0f} L")

    # Sanity: should be in the 3000–4000 range.
    if not (3000 <= fuel <= 4000):
        raise AssertionError(
            f"fuel estimate {fuel:.0f} L outside reasonable range — check unit conversions"
        )
    print("OK")


if __name__ == "__main__":
    main()
