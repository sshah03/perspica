<?php

namespace Example;

use Psr\Log\LoggerInterface;

final class Log
{
    public function __construct(private LoggerInterface $logger) {}

    public function line(string $msg): void
    {
        $this->logger->info($msg);
    }
}
