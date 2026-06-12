// A tiny sliding-window rate limiter for outgoing emoji reactions.
//
// The reaction panel now stays open after a click (issue #15) so users can fire
// several reactions in a row. That makes a runaway / held click — or an
// automated loop — able to flood the room, so we cap the burst client-side:
// at most `max` sends per `windowMs`. Excess clicks are dropped silently (the
// panel stays responsive; the reaction just isn't sent), which feels instant
// for normal use and only bites under genuine spam.
//
// The clock is injectable so the behaviour is deterministically testable
// without touching `Date.now()`.
export class RateLimiter {
  private hits: number[] = [];

  constructor(
    private readonly max: number,
    private readonly windowMs: number,
    private readonly now: () => number = () => Date.now(),
  ) {}

  /** Record an attempt: returns `true` if it is within the limit (and counts
   *  it), `false` if the window is full and the caller should drop it. */
  tryAcquire(): boolean {
    const t = this.now();
    const cutoff = t - this.windowMs;
    // Forget hits that have aged out of the window.
    this.hits = this.hits.filter((h) => h > cutoff);
    if (this.hits.length >= this.max) return false;
    this.hits.push(t);
    return true;
  }
}
