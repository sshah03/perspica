import { Router } from 'express'

export const API_VERSION = '1.0.0'

export function createRouter(): Router {
  const router = Router()

  router.get('/health', (req, res) => {
    res.json({ status: 'ok' })
  })

  return router
}

export interface Config {
  port: number
  host: string
}
