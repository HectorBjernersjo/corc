import { Plugin } from "@opencode/plugin/tui"
import { execFile } from "node:child_process"

export default Plugin.define({
  id: "corc.resume",
  setup(context) {
    const pane = process.env.TMUX_PANE
    if (!pane || !process.env.CORC_MANAGED) return
    let last = ""
    let busy = false
    const report = () => {
      const route = context.ui.router.current()
      if (route.type !== "session") return
      const id = context.data.session.root(route.sessionID) ?? route.sessionID
      if (id === last || busy) return
      busy = true
      execFile("tmux", ["set-option", "-p", "-t", pane, "@corc-session",
        JSON.stringify({ provider: "opencode", id })], (error) => {
        busy = false
        if (!error) last = id
      })
    }
    const timer = setInterval(report, 250)
    report()
    return () => clearInterval(timer)
  },
})
