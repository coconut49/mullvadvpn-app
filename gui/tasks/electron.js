const { spawn } = require('child_process');
const electron = require('electron');

const DISABLE_RESET_NAVIGATION = '--disable-reset-navigation';

let subprocess;

function startElectron(done) {
  const args = [];
  if (process.argv.includes(DISABLE_RESET_NAVIGATION)) {
    args.push(DISABLE_RESET_NAVIGATION);
  }

  subprocess = spawn(electron, ['.', ...args], {
    env: { ...process.env, NODE_ENV: 'development' },
    stdio: 'inherit',
  });
  done();
}

function stopElectron() {
  subprocess.kill();
  return subprocess;
}

function reloadMain(done) {
  stopElectron();
  startElectron(done);
}

function reloadRenderer() {
  subprocess.kill('SIGUSR2');
}

startElectron.displayName = 'start-electron';
reloadMain.displayName = 'reload-main-process';
reloadRenderer.displayName = 'reload-renderer-process';

exports.start = startElectron;
exports.reloadMain = reloadMain;
exports.reloadRenderer = reloadRenderer;
