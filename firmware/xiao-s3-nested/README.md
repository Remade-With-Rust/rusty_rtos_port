# `xiao-s3-nested`: nested interrupts on the Kairos kernel

**Status: PASSES on a XIAO ESP32-S3 (2026-10-01), and its poison arm FAILS,
as it must.**

FreeRTOS's `IntQueue` demo checks that queues stay consistent when
interrupts of different priorities use them, including one interrupting
another. The Kairos sim cannot nest interrupts, so its `IntQueue` arm never
exercised that. Silicon can, so this cell does:

```
NEST low  (level 1) sent 51470 refused 0 received 51470
NEST high (level 3) sent 121533 refused 0 received 121533
NEST nested 49280 (a level-3 interrupt inside a level-1 one); inside a kernel section 0
NEST to_isr: produced 51477 received by low 51462 order_bad 0
NEST consumer order_bad 0 malformed 0
RESULT: PASS
```

The workload:
- **`low`** is a level-1 timer interrupt every 97 µs. It sends to a queue,
  holds a 30 µs window with interrupts open, then receives from a second
  queue.
- **`high`** is a level-3 timer interrupt every 41 µs. It sends to the same
  queue as `low`.
- **Two tasks** sit at the other ends of the queues and check that every
  value arrives in order, per source.

In 5 s:
- 49,280 level-3 interrupts landed inside a level-1 one.
- 173,003 values were sent from the two interrupt levels, and all of them
  arrived in order, with none refused and none lost.
- No level-3 interrupt ever landed inside a kernel section.

## The rule, and the poison that proves the check can fail

Every kernel entry masks to level 3 and **restores the saved `PS`** on the
way out. That is `portSET_INTERRUPT_MASK_FROM_ISR`, with
`configMAX_SYSCALL_INTERRUPT_PRIORITY` at level 3.

`--features poison` makes the level-1 handler enter the kernel unmasked, the
way a handler that believes "an interrupt already has exclusivity" would:

```
NEST low  (level 1) sent 51464 refused 49292 received 2209
NEST high (level 3) sent 121527 refused 116461 received 5029
NEST nested 41865 ...; inside a kernel section 493
NEST consumer order_bad 304 malformed 0
RESULT: FAIL
```

That run showed:
- 493 level-3 interrupts landed inside an unmasked kernel section;
- 304 values were delivered out of order;
- the queue was left corrupted and stuck "full".

That unmasked pattern was live in this repo's `xiao-s3-radio` and
`xiao-s3-wifi` cells, in their tick and switch handlers. It was harmless
only because nothing above level 1 called the kernel in them. Both now mask,
and both re-pass on the board.
