/* Subprocess-local synthetic-kernel adapter; never a physical-device test. */
#ifndef FURAMI_V4L2_FIXTURE_H
#define FURAMI_V4L2_FIXTURE_H
#include <stddef.h>
struct fixture_stats {
    unsigned dequeues, successful_dequeues, requeues, polls, violations;
    unsigned held, mappings, opens, closes, streamoffs, nonblock;
};
/* Exported by the LD_PRELOAD adapter for outcome/ownership observations. */
int furami_fixture_snapshot(struct fixture_stats *stats);
int furami_fixture_produce(unsigned count);
enum fixture_gate_action { FIXTURE_GATE_ARM, FIXTURE_GATE_WAIT, FIXTURE_GATE_RELEASE };
int furami_fixture_qbuf_gate(enum fixture_gate_action action);
#endif
