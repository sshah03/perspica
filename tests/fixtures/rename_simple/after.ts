import { Request, Response } from "express";

function normalizeInput(input: string): string {
    const trimmed = input.trim();
    const normalized = trimmed.toLowerCase();
    return normalized.replace(/\s+/g, "-");
}

function handleRequest(req: Request, res: Response): void {
    const result = normalizeInput(req.body.data);
    res.json({ result });
}
