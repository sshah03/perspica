<?php

namespace Example;

use Psr\Log\LoggerInterface;
use Carbon\Carbon;

final class Log
{
    public function __construct(private LoggerInterface $logger) {}

    public function line(string $msg): void
    {
        $this->logger->info(Carbon::now()->toIso8601String() . ' ' . $msg);
    }
}
