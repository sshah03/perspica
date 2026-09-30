import { Request, Response } from "express";

function processData(input: string): string {
    const trimmed = input.trim();
    const normalized = trimmed.toLowerCase();
    return normalized.replace(/\s+/g, "-");
}

function handleRequest(req: Request, res: Response): void {
    const result = processData(req.body.data);
    res.json({ result });
}
