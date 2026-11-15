import { MoonIcon, SettingsIcon, SunIcon } from 'lucide-react'
import { ThemeProvider, useTheme } from 'next-themes'
import { useRoute, type Route } from '@/hooks'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'
import { TooltipProvider } from '@/components/ui/tooltip'
import { RunsPage } from '@/pages/runs'
import { RunPage } from '@/pages/run'
import { ProjectsPage } from '@/pages/projects'
import { AgentsPage } from '@/pages/agents'
import { ImagesPage } from '@/pages/images'
import { SettingsPage } from '@/pages/settings'

const NAV: { href: string; label: string; match: Route['page'][] }[] = [
  { href: '#/', label: 'Runs', match: ['runs', 'run'] },
  { href: '#/projects', label: 'Projects', match: ['projects'] },
  { href: '#/agents', label: 'Agents', match: ['agents'] },
  { href: '#/images', label: 'Images', match: ['images'] },
]

export default function App() {
  return (
    <ThemeProvider attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange>
      <TooltipProvider>
        <Shell />
        <Toaster position="bottom-right" />
      </TooltipProvider>
    </ThemeProvider>
  )
}

function Shell() {
  const route = useRoute()
  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-30 border-b bg-background/80 backdrop-blur-md">
        <div className="mx-auto flex h-14 max-w-5xl items-center gap-8 px-4 sm:px-6">
          <a href="#/" className="text-[15px] font-semibold tracking-tight">
            moloko
          </a>
          <nav className="flex min-w-0 items-center gap-5 overflow-x-auto text-sm">
            {NAV.map((n) => (
              <a
                key={n.href}
                href={n.href}
                className={cn(
                  'text-muted-foreground transition-colors hover:text-foreground',
                  n.match.includes(route.page) && 'text-foreground',
                )}
              >
                {n.label}
              </a>
            ))}
          </nav>
          <div className="ml-auto flex items-center gap-1">
            <Button variant="ghost" size="icon-sm" className={cn('text-muted-foreground', route.page === 'settings' && 'text-foreground')} asChild>
              <a href="#/settings" aria-label="Settings" title="Settings">
                <SettingsIcon />
              </a>
            </Button>
            <ThemeToggle />
          </div>
        </div>
      </header>
      <main className="mx-auto max-w-5xl px-4 py-12 sm:px-6">
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

function ThemeToggle() {
  const { resolvedTheme, setTheme } = useTheme()
  const dark = resolvedTheme === 'dark'
  return (
    <Button variant="ghost" size="icon-sm" className="text-muted-foreground" onClick={() => setTheme(dark ? 'light' : 'dark')} aria-label="Toggle theme">
      {dark ? <SunIcon /> : <MoonIcon />}
    </Button>
  )
}
