import { useEffect } from 'react'
import { MoonIcon, SettingsIcon, SunIcon } from 'lucide-react'
import { ThemeProvider, useTheme } from 'next-themes'
import { api, isActive } from '@/api'
import { usePoll, useRoute, type Route } from '@/hooks'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip'
import { RunsPage } from '@/pages/runs'
import { RunPage } from '@/pages/run'
import { ProjectsPage } from '@/pages/projects'
import { AgentsPage } from '@/pages/agents'
import { ImagesPage } from '@/pages/images'
import { SettingsPage } from '@/pages/settings'

const NAV: { href: string; label: string; match: Route['page'][] }[] = [
  { href: '/projects', label: 'Projects', match: ['projects'] },
  { href: '/', label: 'Runs', match: ['runs', 'run'] },
  { href: '/images', label: 'Images', match: ['images'] },
  { href: '/deployments', label: 'Deployments', match: ['agents'] },
]

const TITLE: Record<Route['page'], string> = { runs: 'Runs', run: 'Run', projects: 'Projects', agents: 'Deployments', images: 'Images', settings: 'Settings' }

export default function App() {
  return (
    <ThemeProvider attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange>
      <TooltipProvider delayDuration={300}>
        <Shell />
        <Toaster position="bottom-right" />
      </TooltipProvider>
    </ThemeProvider>
  )
}

function Shell() {
  const route = useRoute()
  useEffect(() => {
    document.title = route.page === 'run' ? `Run #${route.id} · moloko` : `${TITLE[route.page]} · moloko`
  }, [route.page, route.page === 'run' && route.id])
  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-30 border-b bg-background/85 backdrop-blur-md">
        <div className="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4 sm:px-6">
          <a href="/" className="flex items-center gap-2.5 text-[15px] font-semibold tracking-tight">
            <Logo />
            moloko
          </a>
          <nav className="flex min-w-0 items-center gap-1 overflow-x-auto text-sm">
            {NAV.map((n) => {
              const current = n.match.includes(route.page)
              return (
                <a
                  key={n.href}
                  href={n.href}
                  aria-current={current ? 'page' : undefined}
                  className={cn(
                    'rounded-md px-2.5 py-1.5 font-medium text-muted-foreground transition-colors hover:text-foreground',
                    current && 'bg-foreground/[0.06] text-foreground dark:bg-foreground/10',
                  )}
                >
                  {n.label}
                </a>
              )
            })}
          </nav>
          <div className="ml-auto flex items-center gap-1">
            <Activity />
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon-sm" className={cn('text-muted-foreground', route.page === 'settings' && 'bg-foreground/[0.06] text-foreground dark:bg-foreground/10')} asChild>
                  <a href="/settings" aria-label="Settings">
                    <SettingsIcon />
                  </a>
                </Button>
              </TooltipTrigger>
              <TooltipContent sideOffset={6}>Settings</TooltipContent>
            </Tooltip>
            <ThemeToggle />
          </div>
        </div>
      </header>
      <main className="mx-auto max-w-6xl px-4 py-10 sm:px-6">
        {route.page === 'run' ? (
          <RunPage id={route.id} job={route.job} />
        ) : route.page === 'projects' ? (
          <ProjectsPage />
        ) : route.page === 'agents' ? (
          <AgentsPage />
        ) : route.page === 'images' ? (
          <ImagesPage />
        ) : route.page === 'settings' ? (
          <SettingsPage />
        ) : (
          <RunsPage />
        )}
      </main>
    </div>
  )
}

function Logo() {
  return (
    <svg viewBox="0 0 24 24" className="size-6" aria-hidden="true">
      <rect width="24" height="24" rx="6" className="fill-foreground" />
      <path d="M6.5 16.5v-6l3 3.2 2.5-3.2 2.5 3.2 3-3.2v6" fill="none" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="stroke-background" />
    </svg>
  )
}

/** Shows on every page that builds are in progress. */
function Activity() {
  const runs = usePoll(() => api.runs(), 5000, [])
  const jobs = runs.data?.flatMap((r) => r.jobs).filter((j) => j.status === 'running').length ?? 0
  const waiting = runs.data?.some((r) => isActive(r.status)) ?? false
  if (!jobs && !waiting) return null
  return (
    <a href="/" className="mr-2 hidden h-7 items-center gap-2 rounded-full border border-running/25 bg-running/10 px-3 text-xs font-medium text-running sm:inline-flex">
      <span className="size-1.5 animate-pulse rounded-full bg-current" />
      {jobs ? `${jobs} ${jobs === 1 ? 'job' : 'jobs'} running` : 'Runs queued'}
    </a>
  )
}

function ThemeToggle() {
  const { resolvedTheme, setTheme } = useTheme()
  const dark = resolvedTheme === 'dark'
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button variant="ghost" size="icon-sm" className="text-muted-foreground" onClick={() => setTheme(dark ? 'light' : 'dark')} aria-label="Toggle theme">
          {dark ? <SunIcon /> : <MoonIcon />}
        </Button>
      </TooltipTrigger>
      <TooltipContent sideOffset={6}>{dark ? 'Light mode' : 'Dark mode'}</TooltipContent>
    </Tooltip>
  )
}
