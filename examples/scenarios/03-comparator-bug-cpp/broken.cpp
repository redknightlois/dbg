// Sorts pending orders by (priority desc, deadline asc) so the
// dispatcher pulls the most urgent first. Production complaint:
// "low-priority items are being dispatched ahead of high-priority
// ones." Reproduce + fix.
//
// Build: c++ -g -O0 -std=c++17 broken.cpp -o broken

#include <algorithm>
#include <cstdint>
#include <iostream>
#include <string>
#include <vector>

struct Order {
    std::string id;
    int priority;          // higher = more urgent
    std::int64_t deadline; // unix ts; lower = sooner
};

bool order_before(const Order& a, const Order& b) {
    // Want: priority DESC, then deadline ASC.
    if (a.priority != b.priority) {
        return a.priority > b.priority; // bug lives on this line
    }
    return a.deadline < b.deadline;
}

int main() {
    std::vector<Order> orders = {
        {"A", 1, 1000},
        {"B", 5, 2000},
        {"C", 3, 500},
        {"D", 5, 1500},
        {"E", 2, 800},
    };

    std::sort(orders.begin(), orders.end(), order_before);

    std::cout << "dispatch order:\n";
    for (const auto& o : orders) {
        std::cout << "  " << o.id << " p=" << o.priority
                  << " d=" << o.deadline << "\n";
    }

    // Sanity: the first dispatched order must have the highest priority.
    if (orders.front().priority != 5) {
        std::cerr << "BUG: front of queue has priority "
                  << orders.front().priority << " (expected 5)\n";
        return 1;
    }
    std::cout << "OK\n";
    return 0;
}
