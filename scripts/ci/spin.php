<?php
// A tiny, deterministic CPU-busy workload for the smoke tests: a known class
// (W) with named methods (a/b) so we can assert the profiler captured them.
class W {
    public function spin(): never { while (true) $this->a(); }
    public function a(): void { $this->b(); }
    public function b(): void { usleep(500); }
}
(new W())->spin();
