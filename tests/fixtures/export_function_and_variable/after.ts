import { Router } from 'express'
import { z } from 'zod'

export const API_VERSION = '2.0.0'

export function createRouter(config: Config): Router {
  const router = Router()

  router.get('/health', (req, res) => {
    res.json({ status: 'ok', version: API_VERSION })
  })

  router.get('/config', (req, res) => {
    res.json(config)
  })

  return router
}

export interface Config {
  port: number
  host: string
  debug: boolean
}
